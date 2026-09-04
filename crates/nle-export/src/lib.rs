//! Cancellable, snapshot-at-start FFmpeg export.
//!
//! The export worker owns source probing and graph construction. It lowers the
//! shared compositor's geometry onto a black project canvas; playback never
//! enters this crate.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use image::ImageEncoder;
use nle_compositor::{
    CompositeLayerInput, CompositeQuad, CompositionRequest, MAX_COMPOSITE_LAYERS, PixelSize,
    plan_composition,
};
use nle_project_io::{PROJECT_TIMEBASE, ProjectSettings, replace_file};
use nle_timeline::{
    AnimatedScalar, AudioEffect, AudioTransition, AudioTransitionKind, BrightnessContrastEffect,
    Clip, ClipId, ColorCurve, CompiledVideoEffectGraph, Fade, KeyframeInterpolation, Tick,
    TimelineSnapshot, TitleOverlay, Track, TrackKind, VideoEffectKind, VideoTransition,
    VideoTransitionKind, VignetteEffect,
};
use nle_title::{TitleRaster, rasterize_title};
use nle_ui_core::{EditorProjectSnapshot, MediaKind, classify_path};

const MAX_EXPORT_VIDEO_CLIPS: usize = 256;
const MAX_EXPORT_AUDIO_CLIPS: usize = 256;
const MAX_EXPORT_TITLES: usize = 256;
const MAX_EXPORT_INPUTS: usize =
    MAX_EXPORT_VIDEO_CLIPS + MAX_EXPORT_AUDIO_CLIPS + MAX_EXPORT_TITLES + 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum H264Encoder {
    Nvidia,
    IntelQuickSync,
    Amd,
    VideoToolbox,
    MediaFoundation,
    OpenH264,
}

impl H264Encoder {
    pub fn ffmpeg_name(self) -> &'static str {
        match self {
            Self::Nvidia => "h264_nvenc",
            Self::IntelQuickSync => "h264_qsv",
            Self::Amd => "h264_amf",
            Self::VideoToolbox => "h264_videotoolbox",
            Self::MediaFoundation => "h264_mf",
            Self::OpenH264 => "libopenh264",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ExportRequest {
    pub snapshot: EditorProjectSnapshot,
    pub settings: ProjectSettings,
    pub output: PathBuf,
    pub ffmpeg: PathBuf,
    pub encoders: Vec<H264Encoder>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ExportEvent {
    /// The encoder process was successfully started. A later event still determines success,
    /// cancellation, or fallback to another encoder.
    EncoderStarted(H264Encoder),
    Progress(f32),
    Completed(PathBuf),
    Cancelled,
    Failed(String),
}

pub struct ExportJob {
    cancel: Arc<AtomicBool>,
    events: mpsc::Receiver<ExportEvent>,
    join: Option<thread::JoinHandle<()>>,
}

impl ExportJob {
    pub fn start(
        request: ExportRequest,
        notify: impl Fn() + Send + Sync + 'static,
    ) -> Result<Self, String> {
        validate_settings(&request.settings)?;
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let (tx, events) = mpsc::channel();
        let notify = Arc::new(notify);
        let join = thread::Builder::new()
            .name("maelstrom-export".into())
            .spawn(move || run_export(request, worker_cancel, tx, notify))
            .map_err(|error| format!("could not start export worker: {error}"))?;
        Ok(Self {
            cancel,
            events,
            join: Some(join),
        })
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
    }

    pub fn try_recv(&self) -> Result<ExportEvent, mpsc::TryRecvError> {
        self.events.try_recv()
    }

    pub fn is_finished(&self) -> bool {
        self.join
            .as_ref()
            .is_none_or(thread::JoinHandle::is_finished)
    }
}

impl Drop for ExportJob {
    fn drop(&mut self) {
        self.cancel();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct MediaProbe {
    source_size: Option<PixelSize>,
    has_audio: bool,
    video_is_av1: bool,
}

#[derive(Clone, Debug)]
struct VideoClipPlan {
    clip: Clip,
    /// Immutable validated effect operations captured once on the export worker.
    effects: CompiledVideoEffectGraph,
    path: PathBuf,
    is_still: bool,
    video_is_av1: bool,
    source_size: PixelSize,
    quad: CompositeQuad,
    input_source_in: Tick,
    input_duration: Tick,
    timeline_start: Tick,
    timeline_end: Tick,
    transition_head: Tick,
    incoming_opacity: Option<TransitionOpacity>,
    incoming_slide: Option<TransitionSlide>,
    incoming_push: Option<TransitionPush>,
    outgoing_push: Option<TransitionPush>,
    outgoing_matte: Option<DipMatte>,
}

#[derive(Clone, Debug)]
struct VideoTrackPlan {
    clips: Vec<VideoClipPlan>,
}

#[derive(Clone, Debug)]
struct AudioClipPlan {
    clip: Clip,
    path: PathBuf,
    has_audio: bool,
    track_gain_db: f32,
    pan: f32,
    /// Active clip effects precede active track effects in signal order.
    effects: Vec<AudioEffect>,
    input_source_in: Tick,
    input_duration: Tick,
    timeline_start: Tick,
    transition_head: Tick,
    transition_envelopes: Vec<AudioTransitionEnvelope>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AudioTransitionRole {
    Outgoing,
    Incoming,
}

#[derive(Clone, Debug)]
struct AudioTransitionEnvelope {
    role: AudioTransitionRole,
    /// Transition-window start relative to the expanded FFmpeg input.
    start: Tick,
    duration: Tick,
}

#[derive(Clone, Debug)]
struct PlannedAudioTransitionEnvelope {
    role: AudioTransitionRole,
    /// Absolute timeline start of the centered transition window.
    start: Tick,
    duration: Tick,
}

#[derive(Clone, Debug, Default)]
struct AudioClipTransitionPlan {
    head: Tick,
    tail: Tick,
    envelopes: Vec<PlannedAudioTransitionEnvelope>,
    incoming_window: Option<Tick>,
    outgoing_window: Option<Tick>,
}

#[derive(Clone, Debug)]
struct TitlePlan {
    title: TitleOverlay,
    raster: TitleRaster,
}

#[derive(Clone, Debug)]
struct TitleAsset {
    title: TitleOverlay,
    path: PathBuf,
}

#[derive(Clone, Debug)]
struct ExportPlan {
    video_tracks: Vec<VideoTrackPlan>,
    audio_clips: Vec<AudioClipPlan>,
    titles: Vec<TitlePlan>,
    duration: Tick,
}

#[derive(Clone, Debug, Default)]
struct ClipTransitionPlan {
    head: Tick,
    tail: Tick,
    incoming_opacity: Option<TransitionOpacity>,
    incoming_slide: Option<TransitionSlide>,
    incoming_push: Option<TransitionPush>,
    outgoing_push: Option<TransitionPush>,
    outgoing_matte: Option<DipMatte>,
    has_outgoing_transition: bool,
    validation_transition: Option<VideoTransition>,
    incoming_window: Option<TransitionWindow>,
    outgoing_window: Option<TransitionWindow>,
}

#[derive(Clone, Debug)]
// Every variant describes the incoming side of a transition; the prefix keeps call sites explicit.
#[allow(clippy::enum_variant_names)]
enum TransitionOpacity {
    /// The existing cross-dissolve envelope begins at the expanded input's local zero.
    IncomingCross(Fade),
    /// Film dissolve uses a gamma-shaped cross-dissolve envelope.
    IncomingFilm(Fade),
    /// Dip-to-black fades the normal incoming clip up from the cut.
    IncomingDip(Fade),
    /// A spatial alpha reveal instead of a whole-frame dissolve.
    IncomingWipe { edge: WipeEdge, fade: Fade },
}

#[derive(Clone, Copy, Debug)]
enum WipeEdge {
    Left,
    Right,
    Top,
    Bottom,
}

#[derive(Clone, Debug)]
struct TransitionSlide {
    edge: WipeEdge,
    fade: Fade,
}

#[derive(Clone, Debug)]
struct TransitionPush {
    edge: WipeEdge,
    fade: Fade,
}

#[derive(Clone, Debug)]
struct DipMatte {
    start: Tick,
    end: Tick,
    duration: Tick,
    outgoing_fade: Fade,
    color: &'static str,
}

#[derive(Clone, Debug)]
struct TransitionWindow {
    duration: Tick,
    transition: VideoTransition,
}

fn plan_video_transitions(
    timeline: &TimelineSnapshot,
    media: &HashMap<u32, (PathBuf, Option<Tick>)>,
) -> Result<HashMap<ClipId, ClipTransitionPlan>, String> {
    let mut plans = HashMap::<ClipId, ClipTransitionPlan>::new();
    let mut transition_ids = HashSet::new();
    for transition in &timeline.transitions {
        if !transition_ids.insert(transition.id) {
            return Err(transition_error(
                transition,
                "duplicates a transition identity",
            ));
        }
        let (left, right) = validated_transition_clips(timeline, transition)?;
        // A disabled edit stays durable and can later be re-enabled, but it
        // contributes no picture. Its transition must not expand either input
        // range or create a matte that would survive the bypass.
        if !left.enabled || !right.enabled {
            continue;
        }
        let left_half = transition.duration.0 / 2;
        let right_half = transition.duration.0 - left_half;
        let dip_matte_start =
            Tick(left.end().0.checked_sub(left_half).ok_or_else(|| {
                transition_error(transition, "dip-to-black matte start underflows")
            })?);
        let outgoing = plans.entry(left.id).or_default();
        if outgoing.has_outgoing_transition {
            return Err(transition_error(
                transition,
                "clip already has an outgoing transition",
            ));
        }
        outgoing.has_outgoing_transition = true;
        outgoing.outgoing_window = Some(TransitionWindow {
            duration: Tick(left_half),
            transition: transition.clone(),
        });
        match transition.kind {
            VideoTransitionKind::CrossDissolve
            | VideoTransitionKind::FilmDissolve
            | VideoTransitionKind::WipeLeft
            | VideoTransitionKind::WipeRight
            | VideoTransitionKind::WipeUp
            | VideoTransitionKind::WipeDown
            | VideoTransitionKind::SlideFromLeft
            | VideoTransitionKind::SlideFromRight
            | VideoTransitionKind::SlideFromTop
            | VideoTransitionKind::SlideFromBottom
            | VideoTransitionKind::PushFromLeft
            | VideoTransitionKind::PushFromRight
            | VideoTransitionKind::PushFromTop
            | VideoTransitionKind::PushFromBottom => {
                outgoing.tail = Tick(outgoing.tail.0.checked_add(right_half).ok_or_else(|| {
                    transition_error(transition, "outgoing input duration overflows")
                })?);
                outgoing.validation_transition = Some(transition.clone());
                if matches!(
                    transition.kind,
                    VideoTransitionKind::PushFromLeft
                        | VideoTransitionKind::PushFromRight
                        | VideoTransitionKind::PushFromTop
                        | VideoTransitionKind::PushFromBottom
                ) {
                    outgoing.outgoing_push = Some(TransitionPush {
                        edge: wipe_edge(transition.kind).expect("push transition has a push edge"),
                        fade: Fade {
                            duration: transition.duration,
                            curve: transition.curve,
                        },
                    });
                }
            }
            VideoTransitionKind::DipToBlack | VideoTransitionKind::DipToWhite => {
                outgoing.outgoing_matte = Some(DipMatte {
                    start: dip_matte_start,
                    end: Tick(
                        dip_matte_start
                            .0
                            .checked_add(transition.duration.0)
                            .ok_or_else(|| {
                                transition_error(transition, "dip-to-black matte end overflows")
                            })?,
                    ),
                    duration: transition.duration,
                    outgoing_fade: Fade {
                        duration: Tick(left_half),
                        curve: transition.curve,
                    },
                    color: match transition.kind {
                        VideoTransitionKind::DipToBlack => "black",
                        VideoTransitionKind::DipToWhite => "white",
                        _ => unreachable!("only dip transitions create a matte"),
                    },
                });
            }
        }
        let incoming = plans.entry(right.id).or_default();
        if incoming.incoming_window.is_some() {
            return Err(transition_error(
                transition,
                "clip already has an incoming transition",
            ));
        }
        incoming.incoming_window = Some(TransitionWindow {
            duration: Tick(right_half),
            transition: transition.clone(),
        });
        match transition.kind {
            VideoTransitionKind::CrossDissolve => {
                incoming.head = Tick(incoming.head.0.checked_add(left_half).ok_or_else(|| {
                    transition_error(transition, "incoming input duration overflows")
                })?);
                incoming.incoming_opacity = Some(TransitionOpacity::IncomingCross(Fade {
                    duration: transition.duration,
                    curve: transition.curve,
                }));
                incoming.validation_transition = Some(transition.clone());
            }
            VideoTransitionKind::FilmDissolve => {
                incoming.head = Tick(incoming.head.0.checked_add(left_half).ok_or_else(|| {
                    transition_error(transition, "incoming input duration overflows")
                })?);
                incoming.incoming_opacity = Some(TransitionOpacity::IncomingFilm(Fade {
                    duration: transition.duration,
                    curve: transition.curve,
                }));
                incoming.validation_transition = Some(transition.clone());
            }
            VideoTransitionKind::WipeLeft
            | VideoTransitionKind::WipeRight
            | VideoTransitionKind::WipeUp
            | VideoTransitionKind::WipeDown => {
                incoming.head = Tick(incoming.head.0.checked_add(left_half).ok_or_else(|| {
                    transition_error(transition, "incoming input duration overflows")
                })?);
                incoming.incoming_opacity = Some(TransitionOpacity::IncomingWipe {
                    edge: wipe_edge(transition.kind).expect("wipe transition has a wipe edge"),
                    fade: Fade {
                        duration: transition.duration,
                        curve: transition.curve,
                    },
                });
                incoming.validation_transition = Some(transition.clone());
            }
            VideoTransitionKind::SlideFromLeft
            | VideoTransitionKind::SlideFromRight
            | VideoTransitionKind::SlideFromTop
            | VideoTransitionKind::SlideFromBottom => {
                incoming.head = Tick(incoming.head.0.checked_add(left_half).ok_or_else(|| {
                    transition_error(transition, "incoming input duration overflows")
                })?);
                incoming.incoming_slide = Some(TransitionSlide {
                    edge: wipe_edge(transition.kind).expect("slide transition has a slide edge"),
                    fade: Fade {
                        duration: transition.duration,
                        curve: transition.curve,
                    },
                });
                incoming.validation_transition = Some(transition.clone());
            }
            VideoTransitionKind::PushFromLeft
            | VideoTransitionKind::PushFromRight
            | VideoTransitionKind::PushFromTop
            | VideoTransitionKind::PushFromBottom => {
                incoming.head = Tick(incoming.head.0.checked_add(left_half).ok_or_else(|| {
                    transition_error(transition, "incoming input duration overflows")
                })?);
                incoming.incoming_push = Some(TransitionPush {
                    edge: wipe_edge(transition.kind).expect("push transition has a push edge"),
                    fade: Fade {
                        duration: transition.duration,
                        curve: transition.curve,
                    },
                });
                incoming.validation_transition = Some(transition.clone());
            }
            VideoTransitionKind::DipToBlack | VideoTransitionKind::DipToWhite => {
                incoming.incoming_opacity = Some(TransitionOpacity::IncomingDip(Fade {
                    duration: Tick(right_half),
                    curve: transition.curve,
                }));
            }
        }
    }
    for track in &timeline.tracks {
        for clip in &track.clips {
            let Some(plan) = plans.get(&clip.id) else {
                continue;
            };
            let incoming = plan
                .incoming_window
                .as_ref()
                .map_or(0, |window| window.duration.0);
            let outgoing = plan
                .outgoing_window
                .as_ref()
                .map_or(0, |window| window.duration.0);
            if incoming.saturating_add(outgoing) > clip.duration.0 {
                let transition = plan
                    .outgoing_window
                    .as_ref()
                    .or(plan.incoming_window.as_ref())
                    .expect("transition plan always has its centered window");
                return Err(transition_error(
                    &transition.transition,
                    &format!(
                        "transition windows overlap inside shared clip {}",
                        clip.id.0
                    ),
                ));
            }
            if plan.head.0 > 0 || plan.tail.0 > 0 {
                validate_video_transition_handle(
                    plan.validation_transition
                        .as_ref()
                        .expect("cross-dissolve source expansion has its transition"),
                    clip,
                    -plan.head.0,
                    plan.head.0.checked_add(plan.tail.0).ok_or_else(|| {
                        format!("clip {} transition source range overflows", clip.id.0)
                    })?,
                    media,
                    "transition",
                )?;
            }
        }
    }
    Ok(plans)
}

fn wipe_edge(kind: VideoTransitionKind) -> Option<WipeEdge> {
    match kind {
        VideoTransitionKind::WipeLeft
        | VideoTransitionKind::SlideFromLeft
        | VideoTransitionKind::PushFromLeft => Some(WipeEdge::Left),
        VideoTransitionKind::WipeRight
        | VideoTransitionKind::SlideFromRight
        | VideoTransitionKind::PushFromRight => Some(WipeEdge::Right),
        VideoTransitionKind::WipeUp
        | VideoTransitionKind::SlideFromTop
        | VideoTransitionKind::PushFromTop => Some(WipeEdge::Top),
        VideoTransitionKind::WipeDown
        | VideoTransitionKind::SlideFromBottom
        | VideoTransitionKind::PushFromBottom => Some(WipeEdge::Bottom),
        VideoTransitionKind::CrossDissolve
        | VideoTransitionKind::FilmDissolve
        | VideoTransitionKind::DipToBlack
        | VideoTransitionKind::DipToWhite => None,
    }
}

fn validated_transition_clips<'a>(
    timeline: &'a TimelineSnapshot,
    transition: &VideoTransition,
) -> Result<(&'a Clip, &'a Clip), String> {
    if transition.id.0 == 0 || transition.duration.0 <= 0 || !transition.curve.is_finite() {
        return Err(transition_error(
            transition,
            "has invalid identity, duration, or curve",
        ));
    }
    let track = timeline
        .tracks
        .iter()
        .find(|track| track.id == transition.track_id)
        .ok_or_else(|| transition_error(transition, "references a missing track"))?;
    if track.kind != TrackKind::Video {
        return Err(transition_error(transition, "must be on a video track"));
    }
    let left = track
        .clips
        .iter()
        .find(|clip| clip.id == transition.left_clip)
        .ok_or_else(|| transition_error(transition, "references a missing outgoing clip"))?;
    let right = track
        .clips
        .iter()
        .find(|clip| clip.id == transition.right_clip)
        .ok_or_else(|| transition_error(transition, "references a missing incoming clip"))?;
    let left_half = transition.duration.0 / 2;
    let right_half = transition.duration.0 - left_half;
    if left.start >= right.start
        || left.end() != right.start
        || left.duration.0 < left_half
        || right.duration.0 < right_half
    {
        return Err(transition_error(
            transition,
            "does not join adjacent clips with sufficient timeline handles",
        ));
    }
    Ok((left, right))
}

fn validate_video_transition_handle(
    transition: &VideoTransition,
    clip: &Clip,
    source_delta: i64,
    extra_duration: i64,
    media: &HashMap<u32, (PathBuf, Option<Tick>)>,
    role: &str,
) -> Result<(), String> {
    let Some((path, saved_duration)) = media.get(&clip.media.0) else {
        return Err(transition_error(
            transition,
            &format!("{role} clip {} references missing media", clip.id.0),
        ));
    };
    if classify_path(path) == MediaKind::Image {
        return Ok(());
    }
    let Some(duration) = saved_duration else {
        return Err(transition_error(
            transition,
            &format!(
                "{role} clip {} has no saved media duration for source-handle validation",
                clip.id.0
            ),
        ));
    };
    let source_in = clip.source_in.0.checked_add(source_delta).ok_or_else(|| {
        transition_error(
            transition,
            &format!("{role} clip {} source range overflows", clip.id.0),
        )
    })?;
    let source_duration = clip.duration.0.checked_add(extra_duration).ok_or_else(|| {
        transition_error(
            transition,
            &format!("{role} clip {} source range overflows", clip.id.0),
        )
    })?;
    let source_end = source_in.checked_add(source_duration).ok_or_else(|| {
        transition_error(
            transition,
            &format!("{role} clip {} source range overflows", clip.id.0),
        )
    })?;
    if source_in < 0 || source_end > duration.0 {
        return Err(transition_error(
            transition,
            &format!("{role} clip {} lacks required source handles", clip.id.0),
        ));
    }
    Ok(())
}

fn transition_error(transition: &VideoTransition, detail: &str) -> String {
    format!(
        "transition {} export is malformed: {detail}",
        transition.id.0
    )
}

fn plan_audio_transitions(
    timeline: &TimelineSnapshot,
    media: &HashMap<u32, (PathBuf, Option<Tick>)>,
) -> Result<HashMap<ClipId, AudioClipTransitionPlan>, String> {
    let mut plans = HashMap::<ClipId, AudioClipTransitionPlan>::new();
    let mut transition_ids = HashSet::new();
    for transition in &timeline.audio_transitions {
        if !transition_ids.insert(transition.id) {
            return Err(audio_transition_error(
                transition,
                "duplicates an audio transition identity",
            ));
        }
        let (left, right) = validated_audio_transition_clips(timeline, transition)?;
        // Do not let a crossfade synthesize audio for a disabled section.
        if !left.enabled || !right.enabled {
            continue;
        }
        let left_half = transition.duration.0 / 2;
        let right_half = transition.duration.0 - left_half;
        validate_audio_transition_handles(media, transition, left, right, left_half, right_half)?;
        let window_start = Tick(left.end().0.checked_sub(left_half).ok_or_else(|| {
            audio_transition_error(transition, "centered window start underflows")
        })?);

        let outgoing = plans.entry(left.id).or_default();
        if outgoing.outgoing_window.is_some() {
            return Err(audio_transition_error(
                transition,
                "outgoing clip already has an audio transition",
            ));
        }
        outgoing.tail = Tick(outgoing.tail.0.checked_add(right_half).ok_or_else(|| {
            audio_transition_error(transition, "outgoing input duration overflows")
        })?);
        outgoing.outgoing_window = Some(Tick(left_half));
        outgoing.envelopes.push(PlannedAudioTransitionEnvelope {
            role: AudioTransitionRole::Outgoing,
            start: window_start,
            duration: transition.duration,
        });

        let incoming = plans.entry(right.id).or_default();
        if incoming.incoming_window.is_some() {
            return Err(audio_transition_error(
                transition,
                "incoming clip already has an audio transition",
            ));
        }
        incoming.head = Tick(incoming.head.0.checked_add(left_half).ok_or_else(|| {
            audio_transition_error(transition, "incoming input duration overflows")
        })?);
        incoming.incoming_window = Some(Tick(right_half));
        incoming.envelopes.push(PlannedAudioTransitionEnvelope {
            role: AudioTransitionRole::Incoming,
            start: window_start,
            duration: transition.duration,
        });
    }

    for track in &timeline.tracks {
        if track.kind != TrackKind::Audio {
            continue;
        }
        for clip in &track.clips {
            let Some(plan) = plans.get(&clip.id) else {
                continue;
            };
            let incoming = plan.incoming_window.map_or(0, |window| window.0);
            let outgoing = plan.outgoing_window.map_or(0, |window| window.0);
            if incoming.saturating_add(outgoing) > clip.duration.0 {
                let transition = timeline
                    .audio_transitions
                    .iter()
                    .find(|item| item.left_clip == clip.id || item.right_clip == clip.id)
                    .expect("planned audio transition references its clip");
                return Err(audio_transition_error(
                    transition,
                    &format!("centered windows overlap inside shared clip {}", clip.id.0),
                ));
            }
        }
    }
    Ok(plans)
}

fn validated_audio_transition_clips<'a>(
    timeline: &'a TimelineSnapshot,
    transition: &AudioTransition,
) -> Result<(&'a Clip, &'a Clip), String> {
    if transition.id.0 == 0
        || transition.duration.0 <= 0
        || transition.kind != AudioTransitionKind::EqualPowerCrossfade
    {
        return Err(audio_transition_error(
            transition,
            "has invalid identity, duration, or kind",
        ));
    }
    let track = timeline
        .tracks
        .iter()
        .find(|track| track.id == transition.track_id)
        .ok_or_else(|| audio_transition_error(transition, "references a missing track"))?;
    if track.kind != TrackKind::Audio {
        return Err(audio_transition_error(
            transition,
            "must be on an audio track",
        ));
    }
    let left = track
        .clips
        .iter()
        .find(|clip| clip.id == transition.left_clip)
        .ok_or_else(|| audio_transition_error(transition, "references a missing outgoing clip"))?;
    let right = track
        .clips
        .iter()
        .find(|clip| clip.id == transition.right_clip)
        .ok_or_else(|| audio_transition_error(transition, "references a missing incoming clip"))?;
    let left_half = transition.duration.0 / 2;
    let right_half = transition.duration.0 - left_half;
    if left.end() != right.start
        || left.start >= right.start
        || left.duration.0 < left_half
        || right.duration.0 < right_half
    {
        return Err(audio_transition_error(
            transition,
            "must join exact adjacent clips with a centered window inside both clips",
        ));
    }
    Ok((left, right))
}

fn validate_audio_transition_handles(
    media: &HashMap<u32, (PathBuf, Option<Tick>)>,
    transition: &AudioTransition,
    left: &Clip,
    right: &Clip,
    left_half: i64,
    right_half: i64,
) -> Result<(), String> {
    if right.source_in.0 < left_half {
        return Err(audio_transition_error(
            transition,
            "incoming source has too few saved frames before its trim",
        ));
    }
    let left_source_end = left
        .source_in
        .0
        .checked_add(left.duration.0)
        .and_then(|value| value.checked_add(right_half))
        .ok_or_else(|| audio_transition_error(transition, "outgoing source range overflows"))?;
    let left_duration = media
        .get(&left.media.0)
        .ok_or_else(|| {
            audio_transition_error(transition, "outgoing clip references missing media")
        })?
        .1
        .ok_or_else(|| {
            audio_transition_error(
                transition,
                "outgoing media duration is unavailable for source-handle validation",
            )
        })?;
    if left_source_end > left_duration.0 {
        return Err(audio_transition_error(
            transition,
            "outgoing source has too few saved frames after its trim",
        ));
    }
    if !media.contains_key(&right.media.0) {
        return Err(audio_transition_error(
            transition,
            "incoming clip references missing media",
        ));
    }
    Ok(())
}

fn audio_transition_error(transition: &AudioTransition, detail: &str) -> String {
    format!(
        "audio transition {} export is malformed: {detail}",
        transition.id.0
    )
}

impl ExportPlan {
    /// Probing happens in the caller's worker, never on the interactive thread.
    fn from_request(request: &ExportRequest) -> Result<Self, String> {
        let ffprobe = request.ffmpeg.with_file_name(if cfg!(windows) {
            "ffprobe.exe"
        } else {
            "ffprobe"
        });
        Self::from_request_with_probe(request, |path| probe_media(&ffprobe, path))
    }

    fn from_request_with_probe(
        request: &ExportRequest,
        mut probe: impl FnMut(&Path) -> Result<MediaProbe, String>,
    ) -> Result<Self, String> {
        validate_settings(&request.settings)?;
        reject_unmapped_audio_effects(&request.snapshot)?;
        let media = request
            .snapshot
            .media
            .iter()
            .map(|item| (item.id, (item.path.clone(), item.duration)))
            .collect::<HashMap<u32, (PathBuf, Option<Tick>)>>();
        let transition_plans = plan_video_transitions(&request.snapshot.timeline, &media)?;
        let audio_transition_plans = plan_audio_transitions(&request.snapshot.timeline, &media)?;
        let mut cache = HashMap::<PathBuf, MediaProbe>::new();
        let mut media_probe = |path: &Path| -> Result<MediaProbe, String> {
            if let Some(value) = cache.get(path) {
                return Ok(*value);
            }
            let value = probe(path)?;
            cache.insert(path.to_owned(), value);
            Ok(value)
        };

        let video_tracks = request
            .snapshot
            .timeline
            .tracks
            .iter()
            .filter(|track| {
                track.kind == TrackKind::Video
                    && !track.muted
                    && track.clips.iter().any(|clip| clip.enabled)
            })
            .collect::<Vec<_>>();
        if video_tracks.len() > MAX_COMPOSITE_LAYERS {
            return Err(format!(
                "export supports at most {MAX_COMPOSITE_LAYERS} unmuted video tracks"
            ));
        }
        let video_count = video_tracks
            .iter()
            .map(|track| track.clips.iter().filter(|clip| clip.enabled).count())
            .sum::<usize>();
        if video_count > MAX_EXPORT_VIDEO_CLIPS {
            return Err(format!(
                "export supports at most {MAX_EXPORT_VIDEO_CLIPS} video clips"
            ));
        }

        let project_size = PixelSize::new(request.settings.size[0], request.settings.size[1]);
        // Sequence duration follows the editor's timeline end, including muted late clips. Those
        // tracks render as black/silence but must not silently shorten the requested sequence.
        let mut duration = request
            .snapshot
            .timeline
            .tracks
            .iter()
            .flat_map(|track| track.clips.iter())
            .map(Clip::end)
            .max()
            .unwrap_or(Tick(0));
        let mut titles = request
            .snapshot
            .timeline
            .titles
            .iter()
            .filter(|title| title.enabled)
            .cloned()
            .collect::<Vec<_>>();
        titles.sort_by_key(|title| (title.z_order, title.start, title.id));
        if titles.len() > MAX_EXPORT_TITLES {
            return Err(format!(
                "export supports at most {MAX_EXPORT_TITLES} enabled titles"
            ));
        }
        let titles = titles
            .into_iter()
            .map(|title| {
                let raster = rasterize_title(&title).map_err(|error| {
                    format!("could not rasterize title {}: {error}", title.id.0)
                })?;
                Ok(TitlePlan { title, raster })
            })
            .collect::<Result<Vec<_>, String>>()?;
        for planned in &titles {
            duration = Tick(duration.0.max(title_end(&planned.title)?.0));
        }
        if video_tracks.is_empty() && titles.is_empty() {
            return Err("timeline has no unmuted video clips or enabled titles".to_owned());
        }
        let mut planned_tracks = Vec::with_capacity(video_tracks.len());
        // Timeline vector order is bottom-to-top, exactly as the monitor's compositor uses it.
        for track in video_tracks {
            let mut clips = Vec::with_capacity(track.clips.len());
            for clip in track.clips.iter().filter(|clip| clip.enabled) {
                let (path, _) = media
                    .get(&clip.media.0)
                    .cloned()
                    .ok_or_else(|| format!("clip {} references missing media", clip.id.0))?;
                let is_still = classify_path(&path) == MediaKind::Image;
                let media_probe = media_probe(&path)?;
                let source_size = media_probe
                    .source_size
                    .ok_or_else(|| format!("clip {} has no decodable video stream", clip.id.0))?;
                let quad = plan_composition(CompositionRequest {
                    project_size,
                    layers: [
                        Some(CompositeLayerInput {
                            clip_id: clip.id,
                            source_size,
                            transform: clip.transform,
                            fade_opacity: 1.0,
                        }),
                        None,
                        None,
                        None,
                    ],
                })
                .and_then(|composition| composition.layers[0])
                .ok_or_else(|| format!("clip {} has invalid composition geometry", clip.id.0))?;
                duration = Tick(duration.0.max(clip.end().0));
                let transition = transition_plans.get(&clip.id).cloned().unwrap_or_default();
                let transition_source = transition.validation_transition.as_ref();
                let input_source_in =
                    clip.source_in
                        .0
                        .checked_sub(transition.head.0)
                        .ok_or_else(|| {
                            transition_source.map_or_else(
                                || format!("clip {} has an invalid input source range", clip.id.0),
                                |item| transition_error(item, "incoming source range underflows"),
                            )
                        })?;
                let input_duration = clip
                    .duration
                    .0
                    .checked_add(transition.head.0)
                    .and_then(|value| value.checked_add(transition.tail.0))
                    .ok_or_else(|| {
                        transition_source.map_or_else(
                            || format!("clip {} has an invalid input duration", clip.id.0),
                            |item| transition_error(item, "input duration overflows"),
                        )
                    })?;
                let timeline_start =
                    clip.start.0.checked_sub(transition.head.0).ok_or_else(|| {
                        transition_source.map_or_else(
                            || format!("clip {} has an invalid timeline start", clip.id.0),
                            |item| transition_error(item, "window start overflows the timeline"),
                        )
                    })?;
                let timeline_end =
                    clip.end().0.checked_add(transition.tail.0).ok_or_else(|| {
                        transition_source.map_or_else(
                            || format!("clip {} has an invalid timeline end", clip.id.0),
                            |item| transition_error(item, "window end overflows the timeline"),
                        )
                    })?;
                let effects = clip.video_effects.compile().map_err(|error| {
                    format!(
                        "clip {} has an invalid video effect graph: {error}",
                        clip.id.0
                    )
                })?;
                clips.push(VideoClipPlan {
                    clip: clip.clone(),
                    effects,
                    path,
                    is_still,
                    video_is_av1: media_probe.video_is_av1,
                    source_size,
                    quad,
                    input_source_in: if is_still {
                        clip.source_in
                    } else {
                        Tick(input_source_in)
                    },
                    input_duration: Tick(input_duration),
                    timeline_start: Tick(timeline_start),
                    timeline_end: Tick(timeline_end),
                    transition_head: transition.head,
                    incoming_opacity: transition.incoming_opacity,
                    incoming_slide: transition.incoming_slide,
                    incoming_push: transition.incoming_push,
                    outgoing_push: transition.outgoing_push,
                    outgoing_matte: transition.outgoing_matte,
                });
            }
            planned_tracks.push(VideoTrackPlan { clips });
        }

        let any_audio_solo = request
            .snapshot
            .timeline
            .tracks
            .iter()
            .any(|track| track.kind == TrackKind::Audio && track.solo);
        let mut audio_clips = Vec::new();
        for track in request
            .snapshot
            .timeline
            .tracks
            .iter()
            .filter(|track| track.audio_is_audible(any_audio_solo))
        {
            for clip in track.clips.iter().filter(|clip| clip.enabled) {
                if audio_clips.len() == MAX_EXPORT_AUDIO_CLIPS {
                    return Err(format!(
                        "export supports at most {MAX_EXPORT_AUDIO_CLIPS} audible audio clips"
                    ));
                }
                let (path, _) = media
                    .get(&clip.media.0)
                    .cloned()
                    .ok_or_else(|| format!("clip {} references missing media", clip.id.0))?;
                let has_audio = media_probe(&path)?.has_audio;
                duration = Tick(duration.0.max(clip.end().0));
                let transition = audio_transition_plans
                    .get(&clip.id)
                    .cloned()
                    .unwrap_or_default();
                let input_source_in =
                    clip.source_in
                        .0
                        .checked_sub(transition.head.0)
                        .ok_or_else(|| {
                            format!(
                                "clip {} audio transition source range underflows",
                                clip.id.0
                            )
                        })?;
                let input_duration = clip
                    .duration
                    .0
                    .checked_add(transition.head.0)
                    .and_then(|value| value.checked_add(transition.tail.0))
                    .ok_or_else(|| {
                        format!("clip {} audio transition duration overflows", clip.id.0)
                    })?;
                let timeline_start =
                    clip.start.0.checked_sub(transition.head.0).ok_or_else(|| {
                        format!(
                            "clip {} audio transition timeline start underflows",
                            clip.id.0
                        )
                    })?;
                let transition_envelopes = transition
                    .envelopes
                    .into_iter()
                    .map(|envelope| {
                        envelope
                            .start
                            .0
                            .checked_sub(timeline_start)
                            .map(|start| AudioTransitionEnvelope {
                                role: envelope.role,
                                start: Tick(start),
                                duration: envelope.duration,
                            })
                            .ok_or_else(|| {
                                format!(
                                    "clip {} audio transition envelope starts before its input",
                                    clip.id.0
                                )
                            })
                    })
                    .collect::<Result<Vec<_>, String>>()?;
                audio_clips.push(AudioClipPlan {
                    clip: clip.clone(),
                    path,
                    has_audio,
                    track_gain_db: track.gain_db,
                    pan: track.pan,
                    effects: clip
                        .effects
                        .iter()
                        .chain(&track.effects)
                        .filter_map(AudioEffect::enabled)
                        .cloned()
                        .collect(),
                    input_source_in: Tick(input_source_in),
                    input_duration: Tick(input_duration),
                    timeline_start: Tick(timeline_start),
                    transition_head: transition.head,
                    transition_envelopes,
                });
            }
        }
        if video_count + audio_clips.len() + titles.len() + 1 > MAX_EXPORT_INPUTS {
            return Err(format!(
                "export graph exceeds {MAX_EXPORT_INPUTS} input limit"
            ));
        }
        Ok(Self {
            video_tracks: planned_tracks,
            audio_clips,
            titles,
            duration,
        })
    }
}

fn validate_settings(settings: &ProjectSettings) -> Result<(), String> {
    if settings.fps[0] == 0
        || settings.fps[1] == 0
        || settings.size[0] == 0
        || settings.size[1] == 0
    {
        Err("invalid export frame rate or dimensions".to_owned())
    } else {
        Ok(())
    }
}

fn reject_unmapped_audio_effects(snapshot: &EditorProjectSnapshot) -> Result<(), String> {
    let any_solo = snapshot
        .timeline
        .tracks
        .iter()
        .any(|track| track.kind == TrackKind::Audio && track.solo);
    if snapshot
        .timeline
        .tracks
        .iter()
        .filter(|track| {
            track.audio_is_audible(any_solo) && track.clips.iter().any(|clip| clip.enabled)
        })
        .any(|track| {
            track
                .effects
                .iter()
                .chain(
                    track
                        .clips
                        .iter()
                        .filter(|clip| clip.enabled)
                        .flat_map(|clip| &clip.effects),
                )
                .filter_map(AudioEffect::enabled)
                .any(|effect| !effect.is_export_supported())
        })
    {
        Err(
            "export cannot render one or more enabled clip or track audio effects; bypass them before exporting"
                .to_owned(),
        )
    } else {
        Ok(())
    }
}

fn run_export(
    request: ExportRequest,
    cancel: Arc<AtomicBool>,
    events: mpsc::Sender<ExportEvent>,
    notify: Arc<dyn Fn() + Send + Sync>,
) {
    let final_output = request.output.clone();
    let staged_output = staged_output_path(&final_output);
    let mut staged_request = request;
    staged_request.output = staged_output.clone();
    let terminal = match ExportPlan::from_request(&staged_request)
        .and_then(|plan| run_export_attempts(&staged_request, &plan, &cancel, &events, &notify))
    {
        Ok(()) => match replace_file(&staged_output, &final_output) {
            Ok(()) => ExportEvent::Completed(final_output),
            Err(error) => {
                let _ = fs::remove_file(&staged_output);
                ExportEvent::Failed(format!("could not commit completed export: {error}"))
            }
        },
        Err(_) if cancel.load(Ordering::Acquire) => {
            let _ = fs::remove_file(&staged_output);
            ExportEvent::Cancelled
        }
        Err(error) => {
            let _ = fs::remove_file(&staged_output);
            ExportEvent::Failed(error)
        }
    };
    let _ = events.send(terminal);
    notify();
}

fn run_export_attempts(
    request: &ExportRequest,
    plan: &ExportPlan,
    cancel: &AtomicBool,
    events: &mpsc::Sender<ExportEvent>,
    notify: &Arc<dyn Fn() + Send + Sync>,
) -> Result<(), String> {
    let title_assets = materialize_title_assets(request, plan, cancel)?;
    let result =
        run_export_attempts_with_assets(request, plan, &title_assets, cancel, events, notify);
    cleanup_title_assets(&title_assets);
    result
}

fn run_export_attempts_with_assets(
    request: &ExportRequest,
    plan: &ExportPlan,
    title_assets: &[TitleAsset],
    cancel: &AtomicBool,
    events: &mpsc::Sender<ExportEvent>,
    notify: &Arc<dyn Fn() + Send + Sync>,
) -> Result<(), String> {
    let encoders = if request.encoders.is_empty() {
        default_h264_encoders()
    } else {
        request.encoders.clone()
    };
    let mut errors = Vec::new();
    for decoder in av1_input_decoder_candidates(plan) {
        for encoder in &encoders {
            if cancel.load(Ordering::Acquire) {
                return Err("export cancelled".to_owned());
            }
            let filter_path = filter_script_path(&request.output, *encoder);
            let (args, filter) = build_ffmpeg_job_with_title_assets_and_input_decoder(
                request,
                plan,
                *encoder,
                title_assets,
                decoder,
            )?;
            if let Err(error) = fs::write(&filter_path, filter) {
                let _ = fs::remove_file(&filter_path);
                return Err(format!("could not write export filter: {error}"));
            }
            let result = run_child_with_encoder(
                &request.ffmpeg,
                &args,
                &filter_path,
                Some(*encoder),
                plan.duration,
                cancel,
                events,
                notify,
            );
            let _ = fs::remove_file(&filter_path);
            match result {
                Ok(()) => return Ok(()),
                Err(error) if cancel.load(Ordering::Acquire) => return Err(error),
                Err(error) => errors.push(export_attempt_failure(decoder, *encoder, &error)),
            }
        }
    }
    let label = if plan_has_av1_video(plan) {
        "all AV1 input-decoder/H.264 encoder attempts failed"
    } else {
        "all H.264 encoders failed"
    };
    Err(format!("{label} ({})", errors.join("; ")))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Av1InputDecoder {
    Default,
    D3d11va,
    Dxva2,
    Cuvid,
    Qsv,
    Libaom,
}

impl Av1InputDecoder {
    fn name(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::D3d11va => "d3d11va",
            Self::Dxva2 => "dxva2",
            Self::Cuvid => "av1_cuvid",
            Self::Qsv => "av1_qsv",
            Self::Libaom => "libaom-av1",
        }
    }

    fn input_args(self) -> &'static [&'static str] {
        match self {
            Self::Default => &[],
            Self::D3d11va => &["-hwaccel", "d3d11va", "-c:v", "av1"],
            Self::Dxva2 => &["-hwaccel", "dxva2", "-c:v", "av1"],
            Self::Cuvid => &["-c:v", "av1_cuvid"],
            Self::Qsv => &["-c:v", "av1_qsv"],
            Self::Libaom => &["-c:v", "libaom-av1"],
        }
    }
}

fn av1_input_decoder_candidates(plan: &ExportPlan) -> Vec<Av1InputDecoder> {
    if !plan_has_av1_video(plan) {
        return vec![Av1InputDecoder::Default];
    }
    #[cfg(windows)]
    {
        vec![
            Av1InputDecoder::D3d11va,
            Av1InputDecoder::Dxva2,
            Av1InputDecoder::Cuvid,
            Av1InputDecoder::Qsv,
            Av1InputDecoder::Libaom,
            Av1InputDecoder::Default,
        ]
    }
    #[cfg(target_os = "linux")]
    {
        vec![
            Av1InputDecoder::Cuvid,
            Av1InputDecoder::Qsv,
            Av1InputDecoder::Libaom,
            Av1InputDecoder::Default,
        ]
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        vec![Av1InputDecoder::Libaom, Av1InputDecoder::Default]
    }
}

fn plan_has_av1_video(plan: &ExportPlan) -> bool {
    plan.video_tracks
        .iter()
        .flat_map(|track| &track.clips)
        .any(|video| video.video_is_av1 && !video.is_still)
}

fn export_attempt_failure(decoder: Av1InputDecoder, encoder: H264Encoder, error: &str) -> String {
    format!(
        "{} input / {} encoder: {error}",
        decoder.name(),
        encoder.ffmpeg_name()
    )
}

fn default_h264_encoders() -> Vec<H264Encoder> {
    #[cfg(target_os = "macos")]
    {
        vec![H264Encoder::VideoToolbox]
    }
    #[cfg(not(target_os = "macos"))]
    {
        vec![H264Encoder::MediaFoundation, H264Encoder::OpenH264]
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn build_ffmpeg_job(
    request: &ExportRequest,
    plan: &ExportPlan,
    encoder: H264Encoder,
) -> Result<(Vec<String>, String), String> {
    build_ffmpeg_job_with_title_assets(request, plan, encoder, &[])
}

fn build_ffmpeg_job_with_title_assets(
    request: &ExportRequest,
    plan: &ExportPlan,
    encoder: H264Encoder,
    title_assets: &[TitleAsset],
) -> Result<(Vec<String>, String), String> {
    build_ffmpeg_job_with_title_assets_and_input_decoder(
        request,
        plan,
        encoder,
        title_assets,
        Av1InputDecoder::Default,
    )
}

fn build_ffmpeg_job_with_title_assets_and_input_decoder(
    request: &ExportRequest,
    plan: &ExportPlan,
    encoder: H264Encoder,
    title_assets: &[TitleAsset],
    av1_decoder: Av1InputDecoder,
) -> Result<(Vec<String>, String), String> {
    build_ffmpeg_job_with_title_assets_and_delivery(
        request,
        plan,
        title_assets,
        VideoDelivery::H264(encoder),
        av1_decoder,
    )
}

#[derive(Clone, Copy, Debug)]
enum VideoDelivery {
    H264(H264Encoder),
    #[cfg(feature = "qualification")]
    RawRgbaFrame {
        timeline_tick: Tick,
    },
}

impl VideoDelivery {
    fn final_format(self) -> &'static str {
        match self {
            Self::H264(_) => "yuv420p",
            #[cfg(feature = "qualification")]
            Self::RawRgbaFrame { .. } => "rgba",
        }
    }
}

fn build_ffmpeg_job_with_title_assets_and_delivery(
    request: &ExportRequest,
    plan: &ExportPlan,
    title_assets: &[TitleAsset],
    delivery: VideoDelivery,
    av1_decoder: Av1InputDecoder,
) -> Result<(Vec<String>, String), String> {
    if title_assets.len() != plan.titles.len() {
        return Err("title export assets do not match the planned titles".to_owned());
    }
    let width = request.settings.size[0];
    let height = request.settings.size[1];
    // Preserve the project ratio for every generated/input video clock. Decimal formatting
    // changes NTSC-style rates before FFmpeg constructs its rational filter time bases.
    let fps = format!("{}/{}", request.settings.fps[0], request.settings.fps[1]);
    let duration = tick_seconds(plan.duration);
    let mut args = vec![
        "-hide_banner".to_owned(),
        "-nostdin".to_owned(),
        "-y".to_owned(),
        "-f".to_owned(),
        "lavfi".to_owned(),
        "-t".to_owned(),
        duration.clone(),
        "-i".to_owned(),
        format!("color=c=black:s={width}x{height}:r={fps}"),
    ];
    let mut input_index = 1usize;
    let mut video_inputs = Vec::new();
    for track in &plan.video_tracks {
        for video in &track.clips {
            if video.is_still {
                // A still has no source-time range to seek. The filter graph freezes its first
                // decoded frame so this works uniformly for every classified image format,
                // including GIF rather than relying on image2's PNG/JPEG-only `-loop` option.
                args.extend([
                    "-t".to_owned(),
                    tick_seconds(video.input_duration),
                    "-i".to_owned(),
                    video.path.to_string_lossy().into_owned(),
                ]);
            } else {
                args.extend([
                    "-ss".to_owned(),
                    tick_seconds(video.input_source_in),
                    // Preserve decoder keyframe preroll and let the graph apply the exact
                    // planned range. This keeps VFR frame selection aligned with preview.
                    "-noaccurate_seek".to_owned(),
                ]);
                if video.video_is_av1 {
                    args.extend(av1_decoder.input_args().iter().map(|arg| (*arg).to_owned()));
                }
                args.extend(["-i".to_owned(), video.path.to_string_lossy().into_owned()]);
            }
            video_inputs.push(input_index);
            input_index += 1;
        }
    }
    let mut audio_inputs = Vec::with_capacity(plan.audio_clips.len());
    for audio in &plan.audio_clips {
        if audio.has_audio {
            args.extend([
                "-ss".to_owned(),
                tick_seconds(audio.input_source_in),
                "-t".to_owned(),
                tick_seconds(audio.input_duration),
                "-i".to_owned(),
                audio.path.to_string_lossy().into_owned(),
            ]);
            audio_inputs.push(Some(input_index));
            input_index += 1;
        } else {
            audio_inputs.push(None);
        }
    }
    let mut title_inputs = Vec::with_capacity(title_assets.len());
    for asset in title_assets {
        args.extend([
            "-loop".to_owned(),
            "1".to_owned(),
            "-framerate".to_owned(),
            fps.clone(),
            "-t".to_owned(),
            tick_seconds(asset.title.duration),
            "-i".to_owned(),
            asset.path.to_string_lossy().into_owned(),
        ]);
        title_inputs.push(input_index);
        input_index += 1;
    }

    let mut filters = vec!["[0:v]format=rgba,setpts=PTS-STARTPTS[vbase0]".to_owned()];
    let mut current_video = "vbase0".to_owned();
    let mut video_number = 0usize;
    for track in &plan.video_tracks {
        for video in &track.clips {
            let layer = format!("vl{video_number}");
            filters.push(video_filter(
                video_inputs[video_number],
                video,
                &fps,
                &layer,
            ));
            let next = format!("vbase{}", video_number + 1);
            let (center_x, center_y) = quad_center(video.quad);
            let (overlay_x, overlay_y) = transition_overlay_position(video, center_x, center_y);
            filters.push(format!(
                "[{current_video}][{layer}]overlay=x='{overlay_x}':y='{overlay_y}':eval=frame:format=rgb:alpha=straight:eof_action=pass:repeatlast=0:enable='between(t,{},{})'[{next}]",
                tick_seconds(video.timeline_start),
                tick_seconds(video.timeline_end)
            ));
            current_video = next;
            if let Some(matte) = &video.outgoing_matte {
                let layer = format!("vdip{video_number}");
                filters.push(dip_matte_filter(width, height, &fps, matte, &layer));
                let next = format!("vbase{}dip", video_number + 1);
                filters.push(format!(
                    "[{current_video}][{layer}]overlay=format=rgb:alpha=straight:eof_action=pass:repeatlast=0:enable='between(t,{},{})'[{next}]",
                    tick_seconds(matte.start),
                    tick_seconds(matte.end),
                ));
                current_video = next;
            }
            video_number += 1;
        }
    }
    for (number, (title, input)) in plan.titles.iter().zip(title_inputs).enumerate() {
        let layer = format!("title{number}");
        filters.push(title_filter(input, &title.title, &fps, &layer));
        let next = format!("vtitle{}", number + 1);
        let center_x = width as f32 * title.title.position_x;
        let center_y = height as f32 * title.title.position_y;
        let end = title_end(&title.title)?;
        filters.push(format!(
            "[{current_video}][{layer}]overlay=x='{center_x:.3}-overlay_w/2':y='{center_y:.3}-overlay_h/2':format=rgb:alpha=straight:eof_action=pass:repeatlast=0:enable='between(t,{},{})'[{next}]",
            tick_seconds(title.title.start),
            tick_seconds(end),
        ));
        current_video = next;
    }
    let final_filter = match delivery {
        VideoDelivery::H264(_) => {
            format!("[{current_video}]format={}[vout]", delivery.final_format())
        }
        #[cfg(feature = "qualification")]
        VideoDelivery::RawRgbaFrame { timeline_tick } => format!(
            "[{current_video}]format={},trim=start={},setpts=PTS-STARTPTS[vout]",
            delivery.final_format(),
            tick_seconds(timeline_tick),
        ),
    };
    filters.push(final_filter);

    if matches!(delivery, VideoDelivery::H264(_)) {
        let mut audio_labels = Vec::with_capacity(plan.audio_clips.len());
        for (number, (audio, input)) in plan.audio_clips.iter().zip(audio_inputs).enumerate() {
            let label = format!("a{number}");
            filters.push(audio_filter(input, audio, &label));
            audio_labels.push(format!("[{label}]"));
        }
        if audio_labels.is_empty() {
            filters.push(format!(
                "anullsrc=r=48000:cl=stereo,{}[aout]",
                audio_output_boundary(plan.duration)
            ));
        } else {
            filters.push(format!(
                "{}amix=inputs={}:normalize=0,{}[aout]",
                audio_labels.join(""),
                audio_labels.len(),
                audio_output_boundary(plan.duration)
            ));
        }
    }
    match delivery {
        VideoDelivery::H264(encoder) => args.extend([
            "-filter_complex_script".to_owned(),
            "FILTER_SCRIPT".to_owned(),
            "-map".to_owned(),
            "[vout]".to_owned(),
            "-map".to_owned(),
            "[aout]".to_owned(),
            "-c:v".to_owned(),
            encoder.ffmpeg_name().to_owned(),
            "-b:v".to_owned(),
            video_bit_rate(width, height).to_owned(),
            "-c:a".to_owned(),
            "aac".to_owned(),
            "-b:a".to_owned(),
            "192k".to_owned(),
            "-movflags".to_owned(),
            "+faststart".to_owned(),
            // Bound the muxed output explicitly. Input `-t` options alone do not guarantee EOF when
            // a filter graph contains generated sources or pass-through overlays.
            "-t".to_owned(),
            duration,
            "-progress".to_owned(),
            "pipe:1".to_owned(),
            "-nostats".to_owned(),
            request.output.to_string_lossy().into_owned(),
        ]),
        #[cfg(feature = "qualification")]
        VideoDelivery::RawRgbaFrame { .. } => args.extend([
            "-filter_complex_script".to_owned(),
            "FILTER_SCRIPT".to_owned(),
            "-map".to_owned(),
            "[vout]".to_owned(),
            "-frames:v".to_owned(),
            "1".to_owned(),
            "-f".to_owned(),
            "rawvideo".to_owned(),
            "-pix_fmt".to_owned(),
            "rgba".to_owned(),
            "pipe:1".to_owned(),
        ]),
    }
    Ok((args, filters.join(";\n")))
}

/// Test-only bridge that evaluates the production export graph without a lossy video codec.
#[cfg(feature = "qualification")]
#[doc(hidden)]
pub fn qualification_render_frame_rgba(
    request: &ExportRequest,
    timeline_tick: Tick,
) -> Result<Vec<u8>, String> {
    if !request.ffmpeg.is_absolute() {
        return Err("qualification FFmpeg path must be absolute".to_owned());
    }
    let plan = ExportPlan::from_request(request)?;
    validate_qualification_tick(timeline_tick, plan.duration)?;
    let expected_bytes = qualification_frame_bytes(request.settings.size)?;
    let title_assets = materialize_title_assets(request, &plan, &AtomicBool::new(false))?;
    let result = (|| {
        let (args, filter) = build_ffmpeg_job_with_title_assets_and_delivery(
            request,
            &plan,
            &title_assets,
            VideoDelivery::RawRgbaFrame { timeline_tick },
            Av1InputDecoder::Default,
        )?;
        let filter_path = write_qualification_filter(&request.output, &filter)?;
        let result = run_qualification_frame(&request.ffmpeg, &args, &filter_path, expected_bytes);
        let _ = fs::remove_file(&filter_path);
        result
    })();
    cleanup_title_assets(&title_assets);
    result
}

#[cfg(feature = "qualification")]
fn validate_qualification_tick(timeline_tick: Tick, duration: Tick) -> Result<(), String> {
    if timeline_tick.0 < 0 || timeline_tick.0 >= duration.0 {
        Err(format!(
            "qualification timeline tick {} is outside export duration {}",
            timeline_tick.0, duration.0
        ))
    } else {
        Ok(())
    }
}

#[cfg(feature = "qualification")]
fn qualification_frame_bytes(size: [u32; 2]) -> Result<usize, String> {
    let bytes = u64::from(size[0])
        .checked_mul(u64::from(size[1]))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| "qualification frame dimensions overflow RGBA byte length".to_owned())?;
    usize::try_from(bytes).map_err(|_| {
        "qualification frame dimensions exceed addressable RGBA byte length".to_owned()
    })
}

#[cfg(feature = "qualification")]
fn write_qualification_filter(output: &Path, filter: &str) -> Result<PathBuf, String> {
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    for attempt in 0..32_u8 {
        let path = parent.join(format!(
            ".maelstrom-qualification-{}-{}-{attempt}.filter",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
        ));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => match std::io::Write::write_all(&mut file, filter.as_bytes()) {
                Ok(()) => return Ok(path),
                Err(error) => {
                    let _ = fs::remove_file(&path);
                    return Err(format!("could not write qualification filter: {error}"));
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("could not create qualification filter: {error}")),
        }
    }
    Err("could not allocate a unique qualification filter script".to_owned())
}

#[cfg(feature = "qualification")]
fn run_qualification_frame(
    ffmpeg: &Path,
    args: &[String],
    filter_path: &Path,
    expected_bytes: usize,
) -> Result<Vec<u8>, String> {
    let filter = filter_path.to_string_lossy();
    let mut child = Command::new(ffmpeg)
        .args(args.iter().map(|arg| {
            if arg == "FILTER_SCRIPT" {
                filter.as_ref()
            } else {
                arg
            }
        }))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not launch {}: {error}", ffmpeg.display()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "missing qualification frame pipe".to_owned())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "missing FFmpeg error pipe".to_owned())?;
    let stdout_join =
        thread::spawn(move || read_limited_stdout(stdout, expected_bytes.saturating_add(1)));
    let stderr_join = thread::spawn(move || read_stderr_tail(stderr));
    let status = child.wait().map_err(|error| error.to_string())?;
    let (frame, overflow) = stdout_join.join().unwrap_or_default();
    let stderr = stderr_join.join().unwrap_or_default();
    if !status.success() {
        return Err(last_error_lines(&String::from_utf8_lossy(&stderr)));
    }
    if overflow || frame.len() != expected_bytes {
        return Err(format!(
            "qualification FFmpeg emitted {} RGBA bytes; expected {expected_bytes}",
            frame.len()
        ));
    }
    Ok(frame)
}

#[cfg(feature = "qualification")]
fn read_limited_stdout(mut reader: impl Read, limit: usize) -> (Vec<u8>, bool) {
    let mut output = Vec::with_capacity(limit.min(1024 * 1024));
    let mut overflow = false;
    let mut buffer = [0_u8; 8192];
    loop {
        let count = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        let available = limit.saturating_sub(output.len());
        output.extend_from_slice(&buffer[..count.min(available)]);
        overflow |= count > available;
    }
    (output, overflow)
}

fn title_filter(input: usize, title: &TitleOverlay, fps: &str, label: &str) -> String {
    let opacity = title_opacity_expression(title);
    format!(
        "[{input}:v]format=rgba,fps={fps},trim=duration={},setpts=PTS-STARTPTS,geq=r='r(X\\,Y)':g='g(X\\,Y)':b='b(X\\,Y)':a='alpha(X\\,Y)*{opacity}',setpts=PTS+{}/TB[{label}]",
        tick_seconds(title.duration),
        tick_seconds(title.start),
    )
}

/// Lower the CPU renderer's linear title envelope into FFmpeg's local-time `T`.
fn title_opacity_expression(title: &TitleOverlay) -> String {
    let mut factors = vec![format!("{:.6}", title.opacity.clamp(0.0, 1.0))];
    if title.fade_in.0 > 0 {
        let duration = tick_seconds(title.fade_in);
        factors.push(format!("if(lt(T\\,{duration})\\,T/{duration}\\,1)"));
    }
    if title.fade_out.0 > 0 {
        let duration = tick_seconds(title.fade_out);
        let start = tick_seconds(Tick((title.duration.0 - title.fade_out.0).max(0)));
        factors.push(format!(
            "if(gt(T\\,{start})\\,({}-T)/{duration}\\,1)",
            tick_seconds(title.duration)
        ));
    }
    factors
        .into_iter()
        .reduce(|left, right| format!("min({left}\\,{right})"))
        .expect("global title opacity is always present")
}

fn title_end(title: &TitleOverlay) -> Result<Tick, String> {
    title
        .start
        .0
        .checked_add(title.duration.0)
        .map(Tick)
        .ok_or_else(|| format!("title {} has an invalid duration", title.id.0))
}

fn materialize_title_assets(
    request: &ExportRequest,
    plan: &ExportPlan,
    cancel: &AtomicBool,
) -> Result<Vec<TitleAsset>, String> {
    let mut assets = Vec::with_capacity(plan.titles.len());
    for (index, planned) in plan.titles.iter().enumerate() {
        if cancel.load(Ordering::Acquire) {
            cleanup_title_assets(&assets);
            return Err("export cancelled".to_owned());
        }
        let path = title_plate_path(&request.output, index);
        let result = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(image::ImageError::IoError)
            .and_then(|file| {
                image::codecs::tga::TgaEncoder::new(file).write_image(
                    &planned.raster.rgba,
                    planned.raster.width,
                    planned.raster.height,
                    image::ExtendedColorType::Rgba8,
                )
            });
        if let Err(error) = result {
            let _ = fs::remove_file(&path);
            cleanup_title_assets(&assets);
            return Err(format!("could not write title plate: {error}"));
        }
        assets.push(TitleAsset {
            title: planned.title.clone(),
            path,
        });
    }
    Ok(assets)
}

fn cleanup_title_assets(assets: &[TitleAsset]) {
    for asset in assets {
        let _ = fs::remove_file(&asset.path);
    }
}

fn video_filter(input: usize, video: &VideoClipPlan, fps: &str, label: &str) -> String {
    let transform = video.clip.transform.clamped();
    // The shared plan is authoritative for post-sizing/post-scale edge lengths. Export only
    // rounds at the raster boundary required by FFmpeg.
    let (scaled_width, scaled_height) = quad_edge_size(video.quad);
    let crop_width = (video.source_size.width as f32
        * (1.0 - transform.crop_left - transform.crop_right))
        .max(1.0);
    let crop_height = (video.source_size.height as f32
        * (1.0 - transform.crop_top - transform.crop_bottom))
        .max(1.0);
    let color = video_color_filter_at_source(&video.effects, video.input_source_in);
    let fades = video_fade_filter(&video.clip, video.timeline_start.0 - video.clip.start.0);
    let transition = video_transition_filter(
        video.incoming_opacity.as_ref(),
        video.transition_head,
        video.clip.duration,
    );
    let angle = transform.rotation_degrees.to_radians();
    let rotation = if angle == 0.0 {
        String::new()
    } else {
        format!(
            ",premultiply=inplace=1,rotate={angle:.9}:c=none:ow=rotw({angle:.9}):oh=roth({angle:.9}),unpremultiply=inplace=1"
        )
    };
    // Freeze exactly the first decoded image frame, then bound the generated stream again in
    // the graph. This makes local `T`, fades, and overlay lifetime match the planned clip.
    let still_prefix = if video.is_still {
        "select='eq(n\\,0)',loop=loop=-1:size=1:start=0,".to_owned()
    } else {
        String::new()
    };
    let input_timing = if video.is_still {
        format!(
            "setpts=PTS-STARTPTS,{still_prefix}fps={fps},trim=duration={}",
            tick_seconds(video.input_duration)
        )
    } else {
        format!(
            "fps={fps}:round=up,trim=start=0:duration={},setpts=PTS-STARTPTS",
            tick_seconds(video.input_duration)
        )
    };
    // Source assets remain straight RGBA at the public boundary. Premultiply before every
    // resampling operation so transparent texels cannot darken filtered edges, return to straight
    // alpha for the existing pointwise color stack. Rotation gets the same guarded round trip;
    // FFmpeg's overlay consumes the final straight-alpha layer.
    format!(
        "[{input}:v]{input_timing},format=rgba,premultiply=inplace=1,crop=w={crop_width:.3}:h={crop_height:.3}:x={:.3}:y={:.3},scale={scaled_width}:{scaled_height}{}{},unpremultiply=inplace=1{color},colorchannelmixer=aa={:.6}{fades}{transition}{rotation},setpts=PTS+{}/TB[{label}]",
        video.source_size.width as f32 * transform.crop_left,
        video.source_size.height as f32 * transform.crop_top,
        if transform.flip_h { ",hflip" } else { "" },
        if transform.flip_v { ",vflip" } else { "" },
        video.quad.opacity,
        tick_seconds(video.timeline_start),
    )
}

/// The mixed stream already contains timeline gaps as samples. Bound padding
/// and trimming by sample count, then rebuild its clock, so missing/invalid EOF
/// timestamps cannot turn trailing silence into an unbounded export.
fn audio_output_boundary(duration: Tick) -> String {
    // Round up to cover the final partial sample (less than 1/48000 second).
    // i128 avoids overflowing on long project durations before rescaling.
    let samples = (i128::from(duration.0.max(0)) * 48_000 + i128::from(PROJECT_TIMEBASE) - 1)
        / i128::from(PROJECT_TIMEBASE);
    format!("apad=whole_len={samples},atrim=end_sample={samples},asetpts=N/SR/TB")
}

fn audio_filter(input: Option<usize>, audio: &AudioClipPlan, label: &str) -> String {
    let duration = tick_seconds(audio.input_duration);
    let delay_samples = audio_delay_samples(audio.timeline_start);
    let track = Track {
        id: audio.clip.track_id,
        kind: TrackKind::Audio,
        muted: false,
        solo: false,
        gain_db: audio.track_gain_db,
        pan: audio.pan,
        effects: Vec::new(),
        clips: Vec::new(),
    };
    let gain = 10.0_f64.powf(audio.clip.mix_gain_db(&track) as f64 / 20.0);
    let left = gain * 10.0_f64.powf(audio.clip.gain_left_db as f64 / 20.0) * pan_left(audio.pan);
    let right = gain * 10.0_f64.powf(audio.clip.gain_right_db as f64 / 20.0) * pan_right(audio.pan);
    let clip_time = if audio.transition_head.0 == 0 {
        "t".to_owned()
    } else {
        format!("(t-{})", tick_seconds(audio.transition_head))
    };
    let fades = audio_fade_expression_at(&audio.clip, &clip_time);
    let transitions = audio_transition_filters(&audio.transition_envelopes);
    let effects = audio_effect_filters(&audio.effects)
        .expect("audio effects were validated before export graph construction");
    match input {
        Some(input) => format!(
            "[{input}:a]asetpts=PTS-STARTPTS,aresample=48000,aformat=channel_layouts=stereo,atrim=duration={duration}{effects},pan=stereo|c0={left:.6}*c0|c1={right:.6}*c1{fades}{transitions},adelay={delay_samples}S:all=1[{label}]"
        ),
        None => format!(
            "anullsrc=r=48000:cl=stereo,atrim=duration={duration},adelay={delay_samples}S:all=1[{label}]"
        ),
    }
}

/// Exact, stack-preserving FFmpeg lowerings for the audio rack subset export
/// can render. The timeline has already excluded bypassed entries here.
fn audio_effect_filters(effects: &[AudioEffect]) -> Result<String, String> {
    let mut filters = String::new();
    for effect in effects {
        let filter = match effect {
            // FFmpeg's highpass/lowpass spelling is width-type `q` plus
            // width `w`; it has no standalone `q` option in our bundled 8.1.
            AudioEffect::HighPass { hz } => format!(
                "highpass=f={}:t=q:w=0.707",
                AudioEffect::effective_filter_hz(*hz)
            ),
            AudioEffect::LowPass { hz } => format!(
                "lowpass=f={}:t=q:w=0.707",
                AudioEffect::effective_filter_hz(*hz)
            ),
            AudioEffect::Eq { hz, db } => format!(
                "equalizer=f={}:t=q:w=1:g={db:.6}",
                AudioEffect::effective_filter_hz(*hz)
            ),
            AudioEffect::StereoWidth { width } => {
                let direct = (1.0 + width) * 0.5;
                let cross = (1.0 - width) * 0.5;
                format!(
                    "pan=stereo|c0={direct:.6}*c0{cross:+.6}*c1|c1={cross:.6}*c0+{direct:.6}*c1"
                )
            }
            _ => {
                return Err(
                    "export cannot render one or more enabled clip or track audio effects; bypass them before exporting"
                        .to_owned(),
                );
            }
        };
        filters.push(',');
        filters.push_str(&filter);
    }
    Ok(filters)
}

fn audio_delay_samples(start: Tick) -> u64 {
    let ticks = i128::from(start.0.max(0));
    ((ticks * 48_000 + i128::from(PROJECT_TIMEBASE / 2)) / i128::from(PROJECT_TIMEBASE))
        .clamp(0, i128::from(u64::MAX)) as u64
}

fn quad_edge_size(quad: CompositeQuad) -> (u32, u32) {
    let width = (quad.positions[1].x - quad.positions[0].x)
        .hypot(quad.positions[1].y - quad.positions[0].y);
    let height = (quad.positions[3].x - quad.positions[0].x)
        .hypot(quad.positions[3].y - quad.positions[0].y);
    (
        width.round().max(1.0) as u32,
        height.round().max(1.0) as u32,
    )
}

fn quad_center(quad: CompositeQuad) -> (f32, f32) {
    (
        (quad.positions[0].x + quad.positions[2].x) * 0.5,
        (quad.positions[0].y + quad.positions[2].y) * 0.5,
    )
}

fn pan_left(pan: f32) -> f64 {
    if pan > 0.0 {
        (1.0 - pan).clamp(0.0, 1.0) as f64
    } else {
        1.0
    }
}

fn pan_right(pan: f32) -> f64 {
    if pan < 0.0 {
        (1.0 + pan).clamp(0.0, 1.0) as f64
    } else {
        1.0
    }
}

/// Applies every enabled color node in canonical stack order. Each node clamps before the next
/// node runs, matching the native viewer rather than algebraically collapsing the stack.
/// The stack runs immediately after `format=rgba`, before opacity, fades, and rotation; alpha
/// passes through unchanged so the effects are purely chromatic.
#[cfg(test)]
fn video_color_filter(clip: &Clip) -> Result<String, String> {
    let effects = clip
        .video_effects
        .compile()
        .map_err(|error| error.to_string())?;
    Ok(video_color_filter_at_source(&effects, clip.source_in))
}

fn video_color_filter_at_source(effects: &CompiledVideoEffectGraph, source_in: Tick) -> String {
    effects
        .iter()
        .filter(|operation| operation.enabled())
        .map(|operation| match operation.kind() {
            VideoEffectKind::BrightnessContrast(effect) => {
                brightness_contrast_filter(effect, source_in)
            }
            VideoEffectKind::Vignette(effect) => vignette_filter(effect, source_in),
        })
        .collect()
}

/// Applies the renderer's normalized radial falloff exactly. `geq` evaluates each channel
/// separately, so the shared luminance multiplier is repeated while alpha is passed through.
fn vignette_filter(effect: &VignetteEffect, source_in: Tick) -> String {
    let amount = animated_scalar_expression(&effect.amount, source_in, "T");
    let midpoint = animated_scalar_expression(&effect.midpoint, source_in, "T");
    let feather = animated_scalar_expression(&effect.feather, source_in, "T");
    let center_x = animated_scalar_expression(&effect.center_x, source_in, "T");
    let center_y = animated_scalar_expression(&effect.center_y, source_in, "T");
    let multiplier = format!(
        "st(0\\,2*((X+0.5)/W-(0.5+({center_x})*0.5)));\
         st(1\\,2*((Y+0.5)/H-(0.5+({center_y})*0.5)));\
         st(2\\,sqrt(ld(0)*ld(0)+ld(1)*ld(1))/sqrt(2));\
         st(3\\,({midpoint})+({feather})*(1-({midpoint})));\
         st(4\\,max(0\\,min(1\\,(ld(2)-({midpoint}))/max(0.0001\\,ld(3)-({midpoint})))));\
         st(5\\,ld(4)*ld(4)*(3-2*ld(4)));\
         (1-({amount})*ld(5))"
    );
    format!(
        ",geq=r='r(X\\,Y)*({multiplier})':g='g(X\\,Y)*({multiplier})':b='b(X\\,Y)*({multiplier})':a='alpha(X\\,Y)'"
    )
}

fn brightness_contrast_filter(effect: &BrightnessContrastEffect, source_in: Tick) -> String {
    let brightness = animated_scalar_expression(&effect.brightness, source_in, "T");
    let contrast = animated_scalar_expression(&effect.contrast, source_in, "T");
    let temperature = animated_scalar_expression(&effect.temperature, source_in, "T");
    let tint = animated_scalar_expression(&effect.tint, source_in, "T");
    let saturation = animated_scalar_expression(&effect.saturation, source_in, "T");
    let exposure = animated_scalar_expression(&effect.exposure, source_in, "T");
    let highlights = animated_scalar_expression(&effect.highlights, source_in, "T");
    let shadows = animated_scalar_expression(&effect.shadows, source_in, "T");
    let whites = animated_scalar_expression(&effect.whites, source_in, "T");
    let blacks = animated_scalar_expression(&effect.blacks, source_in, "T");
    let temperature_tint =
        |component: &str, offset: &str| format!("({component}(X\\,Y)/255+{offset})");
    let red = temperature_tint("r", &format!("0.10*({temperature})+0.05*({tint})"));
    let green = temperature_tint("g", &format!("-0.05*({tint})"));
    let blue = temperature_tint("b", &format!("-0.10*({temperature})+0.05*({tint})"));
    let exposure_scale = format!("pow(2\\,({exposure}))");
    let red = format!("({red})*{exposure_scale}");
    let green = format!("({green})*{exposure_scale}");
    let blue = format!("({blue})*{exposure_scale}");
    // FFmpeg expression registers keep the generated script compact even with the full ten-node
    // correction stack. Saturation preserves luma, so
    // the post-contrast luma in register 4 can be derived directly from register 3. The tonal
    // order matches the native viewer: broad Highlights/Shadows quadratic masks, then narrower
    // Whites/Blacks eighth-power masks. All controls are normalized encoded-sRGB offsets.
    let channel = |component_register: u8| {
        format!(
            "st(0\\,{red});st(1\\,{green});st(2\\,{blue});\
             st(3\\,0.2126*ld(0)+0.7152*ld(1)+0.0722*ld(2));\
             st(4\\,max(0\\,min(1\\,(ld(3)-0.5)*({contrast})+0.5+({brightness}))));\
             max(0\\,min(255\\,(((ld(3)+(ld({component_register})-ld(3))*({saturation})-0.5)*({contrast})+0.5+({brightness}))+\
             0.25*({highlights})*ld(4)*ld(4)+0.25*({shadows})*(1-ld(4))*(1-ld(4))+\
             0.20*({whites})*pow(ld(4)\\,8)+0.20*({blacks})*pow(1-ld(4)\\,8))*255))"
        )
    };
    let basic = format!(
        ",geq=r='{}':g='{}':b='{}':a='alpha(X\\,Y)'",
        channel(0),
        channel(1),
        channel(2),
    );
    if effect.curves.is_identity() {
        basic
    } else {
        format!(
            "{basic},curves=interp=natural:master='{}':red='{}':green='{}':blue='{}'",
            ffmpeg_curve_points(&effect.curves.master),
            ffmpeg_curve_points(&effect.curves.red),
            ffmpeg_curve_points(&effect.curves.green),
            ffmpeg_curve_points(&effect.curves.blue),
        )
    }
}

fn ffmpeg_curve_points(curve: &ColorCurve) -> String {
    curve
        .points
        .iter()
        .map(|point| {
            format!(
                "{}/{}",
                compact_curve_number(point.x),
                compact_curve_number(point.y)
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn compact_curve_number(value: f32) -> String {
    let mut value = format!("{value:.6}");
    while value.ends_with('0') {
        value.pop();
    }
    if value.ends_with('.') {
        value.pop();
    }
    value
}

fn video_fade_filter(clip: &Clip, time_offset: i64) -> String {
    let time = if time_offset == 0 {
        "T".to_owned()
    } else if time_offset > 0 {
        format!("(T+{})", tick_seconds(Tick(time_offset)))
    } else {
        format!("(T-{})", tick_seconds(Tick(-time_offset)))
    };
    let fade = combined_fade_expression(clip, &time, true);
    if fade == "1" {
        String::new()
    } else {
        format!(",geq=r='r(X\\,Y)':g='g(X\\,Y)':b='b(X\\,Y)':a='alpha(X\\,Y)*{fade}'")
    }
}

/// Transition envelopes use raw quadratic opacity, deliberately without the monitor's
/// video-fade gamma/cutoff response. Cross-dissolve retains its historical local-time graph.
fn video_transition_filter(
    incoming: Option<&TransitionOpacity>,
    head: Tick,
    clip_duration: Tick,
) -> String {
    incoming
        .into_iter()
        .map(|role| {
            let opacity = match role {
                TransitionOpacity::IncomingCross(fade) => {
                    fade_expression(*fade, "T", false, fade.duration, false)
                }
                TransitionOpacity::IncomingDip(fade) => fade_expression(
                    *fade,
                    &transition_normal_time(head),
                    false,
                    clip_duration,
                    false,
                ),
                TransitionOpacity::IncomingFilm(fade) => {
                    let base = fade_expression(*fade, "T", false, fade.duration, false);
                    format!("pow(({base}),0.650000)")
                }
                TransitionOpacity::IncomingWipe { edge, fade } => {
                    let progress = fade_expression(*fade, "T", false, fade.duration, false);
                    wipe_alpha_expression(*edge, &progress)
                }
            };
            format!(",geq=r='r(X\\,Y)':g='g(X\\,Y)':b='b(X\\,Y)':a='alpha(X\\,Y)*{opacity}'")
        })
        .collect()
}

fn wipe_alpha_expression(edge: WipeEdge, progress: &str) -> String {
    match edge {
        WipeEdge::Left => format!("if(lte((X+0.5)/W\\,({progress}))\\,1\\,0)"),
        WipeEdge::Right => format!("if(gte((X+0.5)/W\\,1-({progress}))\\,1\\,0)"),
        WipeEdge::Top => format!("if(lte((Y+0.5)/H\\,({progress}))\\,1\\,0)"),
        WipeEdge::Bottom => format!("if(gte((Y+0.5)/H\\,1-({progress}))\\,1\\,0)"),
    }
}

fn transition_overlay_position(
    video: &VideoClipPlan,
    center_x: f32,
    center_y: f32,
) -> (String, String) {
    let mut x = format!("{center_x:.3}-overlay_w/2");
    let mut y = format!("{center_y:.3}-overlay_h/2");
    if let Some(slide) = &video.incoming_slide {
        let time = format!("(t-{})", tick_seconds(video.timeline_start));
        let progress = fade_expression(slide.fade, &time, false, slide.fade.duration, false);
        match slide.edge {
            WipeEdge::Left => x = format!("{x}-overlay_w*(1-({progress}))"),
            WipeEdge::Right => x = format!("{x}+overlay_w*(1-({progress}))"),
            WipeEdge::Top => y = format!("{y}-overlay_h*(1-({progress}))"),
            WipeEdge::Bottom => y = format!("{y}+overlay_h*(1-({progress}))"),
        }
    }
    transition_push_overlay_position(video, x, y)
}

fn transition_push_overlay_position(
    video: &VideoClipPlan,
    mut x: String,
    mut y: String,
) -> (String, String) {
    if let Some(push) = &video.incoming_push {
        let time = format!("(t-{})", tick_seconds(video.timeline_start));
        let progress = fade_expression(push.fade, &time, false, push.fade.duration, false);
        match push.edge {
            WipeEdge::Left => x = format!("{x}+overlay_w*(({progress})-1)"),
            WipeEdge::Right => x = format!("{x}+overlay_w*(1-({progress}))"),
            WipeEdge::Top => y = format!("{y}+overlay_h*(({progress})-1)"),
            WipeEdge::Bottom => y = format!("{y}+overlay_h*(1-({progress}))"),
        }
    }
    if let Some(push) = &video.outgoing_push {
        let start = Tick(video.timeline_end.0 - push.fade.duration.0);
        let time = format!("(t-{})", tick_seconds(start));
        let progress = fade_expression(push.fade, &time, false, push.fade.duration, false);
        match push.edge {
            WipeEdge::Left => x = format!("{x}+overlay_w*({progress})"),
            WipeEdge::Right => x = format!("{x}-overlay_w*({progress})"),
            WipeEdge::Top => y = format!("{y}+overlay_h*({progress})"),
            WipeEdge::Bottom => y = format!("{y}-overlay_h*({progress})"),
        }
    }
    (x, y)
}

fn dip_matte_filter(width: u32, height: u32, fps: &str, matte: &DipMatte, label: &str) -> String {
    let opacity = if matte.outgoing_fade.duration.0 == 0 {
        "1".to_owned()
    } else {
        let outgoing = fade_expression(
            matte.outgoing_fade,
            "T",
            true,
            matte.outgoing_fade.duration,
            false,
        );
        format!("1-({outgoing})")
    };
    format!(
        "color=c={}:s={width}x{height}:r={fps},format=rgba,trim=duration={},setpts=PTS-STARTPTS,geq=r='r(X\\,Y)':g='g(X\\,Y)':b='b(X\\,Y)':a='alpha(X\\,Y)*{opacity}',setpts=PTS+{}/TB[{label}]",
        matte.color,
        tick_seconds(matte.duration),
        tick_seconds(matte.start),
    )
}

fn transition_normal_time(head: Tick) -> String {
    if head.0 == 0 {
        "T".to_owned()
    } else {
        format!("(T-{})", tick_seconds(head))
    }
}

/// Produces a source-time expression for FFmpeg's per-frame `T`.  Each key's interpolation
/// describes the segment that begins at that key; values hold before the first and after the
/// last key just like `AnimatedScalar::evaluate`.
fn animated_scalar_expression(value: &AnimatedScalar, source_in: Tick, time: &str) -> String {
    let source_time = format!("({time}+{})", tick_seconds(source_in));
    let Some(first) = value.keyframes.first() else {
        return format!("{:.6}", value.value);
    };
    let mut expression = format!(
        "{:.6}",
        value.keyframes.last().expect("first key exists").value
    );
    for index in (0..value.keyframes.len() - 1).rev() {
        let key = &value.keyframes[index];
        let next = &value.keyframes[index + 1];
        let key_time = tick_seconds(key.source_tick);
        let next_time = tick_seconds(next.source_tick);
        let segment = match key.interpolation {
            KeyframeInterpolation::Hold => format!("{:.6}", key.value),
            KeyframeInterpolation::Linear => format!(
                "{:.6}+({:.6}-{:.6})*(({source_time}-{key_time})/({next_time}-{key_time}))",
                key.value, next.value, key.value
            ),
            KeyframeInterpolation::Smooth => format!(
                "{:.6}+({:.6}-{:.6})*((({source_time}-{key_time})/({next_time}-{key_time}))*(({source_time}-{key_time})/({next_time}-{key_time}))*(3-2*(({source_time}-{key_time})/({next_time}-{key_time}))))",
                key.value, next.value, key.value
            ),
            KeyframeInterpolation::EaseIn => format!(
                "{:.6}+({:.6}-{:.6})*((({source_time}-{key_time})/({next_time}-{key_time}))*(({source_time}-{key_time})/({next_time}-{key_time})))",
                key.value, next.value, key.value
            ),
            KeyframeInterpolation::EaseOut => format!(
                "{:.6}+({:.6}-{:.6})*(1-(1-(({source_time}-{key_time})/({next_time}-{key_time})))*(1-(({source_time}-{key_time})/({next_time}-{key_time}))))",
                key.value, next.value, key.value
            ),
        };
        expression = format!("if(lt({source_time}\\,{next_time})\\,{segment}\\,{expression})");
    }
    // The first guard avoids evaluating the segment expression before its source-time domain.
    format!(
        "if(lt({source_time}\\,{})\\,{:.6}\\,{expression})",
        tick_seconds(first.source_tick),
        first.value
    )
}

fn audio_fade_expression_at(clip: &Clip, time: &str) -> String {
    let fade = combined_fade_expression(clip, time, false);
    if fade != "1" {
        format!(",volume='{fade}':eval=frame")
    } else {
        String::new()
    }
}

fn audio_transition_filters(envelopes: &[AudioTransitionEnvelope]) -> String {
    envelopes
        .iter()
        .map(|envelope| {
            let direction = match envelope.role {
                AudioTransitionRole::Outgoing => "out",
                AudioTransitionRole::Incoming => "in",
            };
            format!(
                ",afade=t={direction}:st={}:d={}:curve=qsin",
                tick_seconds(envelope.start),
                tick_seconds(envelope.duration),
            )
        })
        .collect()
}

/// FFmpeg expression equivalent of the shared quadratic fade contract. Commas are escaped for
/// the filter graph grammar; video additionally applies the monitor's gamma/cutoff contract.
fn combined_fade_expression(clip: &Clip, time: &str, video: bool) -> String {
    let mut expressions = Vec::new();
    if clip.fade_in.duration.0 > 0 {
        expressions.push(fade_expression(
            clip.fade_in,
            time,
            false,
            clip.duration,
            video,
        ));
    }
    if clip.fade_out.duration.0 > 0 {
        expressions.push(fade_expression(
            clip.fade_out,
            time,
            true,
            clip.duration,
            video,
        ));
    }
    expressions
        .into_iter()
        .reduce(|left, right| format!("min({left}\\,{right})"))
        .unwrap_or_else(|| "1".to_owned())
}

fn fade_expression(
    fade: Fade,
    time: &str,
    outward: bool,
    clip_duration: Tick,
    video: bool,
) -> String {
    let duration = tick_seconds(fade.duration);
    let progress = if outward {
        format!("({}-{})/{duration}", tick_seconds(clip_duration), time)
    } else {
        format!("{time}/{duration}")
    };
    let control = 0.5 + fade.curve.clamp(-1.0, 1.0) * 0.5;
    let envelope =
        format!("(2*{control:.6}*({progress})+(1-2*{control:.6})*pow(({progress})\\,2))");
    let value = if video {
        format!("max(0\\,min(1\\,(pow({envelope}\\,1.5)-0.08)/0.92))")
    } else {
        format!("max(0\\,min(1\\,{envelope}))")
    };
    if outward {
        let start = tick_seconds(Tick((clip_duration.0 - fade.duration.0).max(0)));
        format!("if(gt({time}\\,{start})\\,{value}\\,1)")
    } else {
        format!("if(lt({time}\\,{duration})\\,{value}\\,1)")
    }
}

fn probe_media(ffprobe: &Path, path: &Path) -> Result<MediaProbe, String> {
    let video = Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height,codec_name",
            "-of",
            "default=noprint_wrappers=1",
        ])
        .arg(path)
        .output()
        .map_err(|error| format!("could not launch {}: {error}", ffprobe.display()))?;
    let source_size = if video.status.success() {
        let video_stdout = String::from_utf8_lossy(&video.stdout);
        let value = |name| {
            video_stdout
                .lines()
                .find_map(|line| line.strip_prefix(name))
        };
        match (value("width="), value("height=")) {
            (Some(width), Some(height)) => match (width.parse::<u32>(), height.parse::<u32>()) {
                (Ok(width), Ok(height)) if width > 0 && height > 0 => {
                    Some(PixelSize::new(width, height))
                }
                _ => None,
            },
            _ => None,
        }
    } else {
        None
    };
    let audio = Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-select_streams",
            "a:0",
            "-show_entries",
            "stream=index",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .map_err(|error| format!("could not launch {}: {error}", ffprobe.display()))?;
    Ok(MediaProbe {
        source_size,
        has_audio: audio.status.success() && !audio.stdout.is_empty(),
        video_is_av1: video.status.success()
            && String::from_utf8_lossy(&video.stdout)
                .lines()
                .find_map(|line| line.strip_prefix("codec_name="))
                .is_some_and(|codec| codec.eq_ignore_ascii_case("av1")),
    })
}

#[cfg(test)]
fn run_child(
    ffmpeg: &Path,
    args: &[String],
    filter_path: &Path,
    duration: Tick,
    cancel: &AtomicBool,
    events: &mpsc::Sender<ExportEvent>,
    notify: &Arc<dyn Fn() + Send + Sync>,
) -> Result<(), String> {
    run_child_with_encoder(
        ffmpeg,
        args,
        filter_path,
        None,
        duration,
        cancel,
        events,
        notify,
    )
}

// Keep process, progress, cancellation, and notification ownership explicit at the FFmpeg boundary.
#[allow(clippy::too_many_arguments)]
fn run_child_with_encoder(
    ffmpeg: &Path,
    args: &[String],
    filter_path: &Path,
    encoder: Option<H264Encoder>,
    duration: Tick,
    cancel: &AtomicBool,
    events: &mpsc::Sender<ExportEvent>,
    notify: &Arc<dyn Fn() + Send + Sync>,
) -> Result<(), String> {
    let filter = filter_path.to_string_lossy();
    let mut child = Command::new(ffmpeg)
        .args(args.iter().map(|arg| {
            if arg == "FILTER_SCRIPT" {
                filter.as_ref()
            } else {
                arg.as_str()
            }
        }))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not launch {}: {error}", ffmpeg.display()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "missing FFmpeg progress pipe".to_owned())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "missing FFmpeg error pipe".to_owned())?;
    if let Some(encoder) = encoder {
        let _ = events.send(ExportEvent::EncoderStarted(encoder));
        notify();
    }
    let (line_tx, line_rx) = mpsc::channel();
    let stdout_join = thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let _ = line_tx.send(line);
        }
    });
    let stderr_join = thread::spawn(move || read_stderr_tail(stderr));
    let status = wait_for_child(&mut child, &line_rx, duration, cancel, events, notify);
    if status.is_err() {
        // Even an OS polling error must release the exact child and its pipes.
        let _ = child.kill();
        let _ = child.wait();
    }
    let _ = stdout_join.join();
    let stderr = stderr_join.join().unwrap_or_default();
    if status?.success() {
        Ok(())
    } else {
        Err(last_error_lines(&String::from_utf8_lossy(&stderr)))
    }
}

const MAX_STDERR_TAIL_BYTES: usize = 64 * 1024;

/// Always drain FFmpeg diagnostics, but retain only a bounded tail. A stream of
/// repeated timestamp warnings must not grow the application's memory forever.
fn read_stderr_tail(mut reader: impl Read) -> Vec<u8> {
    let mut tail = VecDeque::with_capacity(MAX_STDERR_TAIL_BYTES);
    let mut buffer = [0_u8; 8192];
    loop {
        let count = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        let excess = (tail.len() + count).saturating_sub(MAX_STDERR_TAIL_BYTES);
        tail.drain(..excess);
        tail.extend(&buffer[..count]);
    }
    tail.into_iter().collect()
}

fn wait_for_child(
    child: &mut Child,
    lines: &mpsc::Receiver<String>,
    duration: Tick,
    cancel: &AtomicBool,
    events: &mpsc::Sender<ExportEvent>,
    notify: &Arc<dyn Fn() + Send + Sync>,
) -> Result<std::process::ExitStatus, String> {
    loop {
        if cancel.load(Ordering::Acquire) {
            let _ = child.kill();
            return child.wait().map_err(|error| error.to_string());
        }
        while let Ok(line) = lines.try_recv() {
            if let Some(value) = line.strip_prefix("out_time_us=")
                && let Ok(microseconds) = value.parse::<f64>()
            {
                let progress = (microseconds / duration.0.max(1) as f64).clamp(0.0, 1.0) as f32;
                let _ = events.send(ExportEvent::Progress(progress));
                notify();
            }
        }
        if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
            return Ok(status);
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn tick_seconds(tick: Tick) -> String {
    format!("{:.6}", tick.0.max(0) as f64 / PROJECT_TIMEBASE as f64)
}

fn video_bit_rate(width: u32, height: u32) -> &'static str {
    match u64::from(width) * u64::from(height) {
        pixels if pixels >= 7680 * 4320 => "80M",
        pixels if pixels >= 3840 * 2160 => "35M",
        pixels if pixels >= 1920 * 1080 => "12M",
        _ => "6M",
    }
}

fn filter_script_path(output: &Path, encoder: H264Encoder) -> PathBuf {
    output
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!(
            ".maelstrom-export-{}-{}-{}.filter",
            std::process::id(),
            encoder.ffmpeg_name(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ))
}

fn title_plate_path(output: &Path, index: usize) -> PathBuf {
    output
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!(
            ".maelstrom-title-{}-{}-{index}.tga",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ))
}

fn staged_output_path(output: &Path) -> PathBuf {
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    let stem = output
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("export");
    let extension = output
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("mp4");
    parent.join(format!(
        ".{stem}.maelstrom-{}-{}.staged.{extension}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ))
}

fn last_error_lines(stderr: &str) -> String {
    let lines = stderr.lines().rev().take(4).collect::<Vec<_>>();
    if lines.is_empty() {
        "FFmpeg exited without an error message".to_owned()
    } else {
        lines.into_iter().rev().collect::<Vec<_>>().join(" | ")
    }
}

#[cfg(test)]
mod audio_boundary_tests;

#[cfg(test)]
mod vfr_export_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use nle_compositor::video_fade_opacity;
    use nle_timeline::{
        AnimatedScalar, AudioEffect, BrightnessContrastEffect, ClipSizingMode, ClipTransform,
        ColorCurve, CurvePoint, KeyframeInterpolation, MediaId, ScalarKeyframe, TitleColor,
        TitleId, VideoEffectId, VideoEffectKind, VideoEffectNode, VignetteEffect,
    };
    use nle_title::title_fade_opacity;
    use nle_ui_core::{EditorState, Language};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempFiles(Vec<PathBuf>);

    // The project-built FFmpeg may create many internal worker threads. Running every integration
    // encode concurrently can exhaust Windows process resources and make a tiny finite job spin.
    // Serialize only these external-process tests; ordinary pure-Rust tests remain parallel.
    static REAL_FFMPEG_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    pub(super) fn real_ffmpeg_test_guard() -> std::sync::MutexGuard<'static, ()> {
        REAL_FFMPEG_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn ffmpeg_error_reader_drains_large_logs_but_retains_only_a_bounded_tail() {
        let final_errors = b"\nfirst\nsecond\nthird\nfinal error\n";
        let input = std::io::repeat(b'x')
            .take((MAX_STDERR_TAIL_BYTES * 256) as u64)
            .chain(final_errors.as_slice());
        let tail = read_stderr_tail(input);
        assert_eq!(tail.len(), MAX_STDERR_TAIL_BYTES);
        assert!(tail.ends_with(final_errors));
        assert_eq!(
            last_error_lines(&String::from_utf8_lossy(&tail)),
            "first | second | third | final error"
        );
    }

    #[test]
    fn ffmpeg_error_reader_preserves_empty_short_and_non_utf8_diagnostics() {
        assert_eq!(
            last_error_lines(&String::from_utf8_lossy(&read_stderr_tail(&b""[..]))),
            "FFmpeg exited without an error message"
        );
        let input = b"codec failed: \xff\npath: \xe6\xb5\xb7\n";
        assert_eq!(read_stderr_tail(input.as_slice()), input);
        assert_eq!(
            last_error_lines(&String::from_utf8_lossy(input)),
            "codec failed: \u{fffd} | path: 海"
        );
    }

    impl Drop for TempFiles {
        fn drop(&mut self) {
            for path in &self.0 {
                let _ = fs::remove_file(path);
            }
        }
    }

    fn request(editor: &EditorState) -> ExportRequest {
        ExportRequest {
            snapshot: editor.snapshot(),
            settings: ProjectSettings::default(),
            output: PathBuf::from("output.mp4"),
            ffmpeg: PathBuf::from("ffmpeg"),
            encoders: vec![H264Encoder::OpenH264],
        }
    }

    fn probe(_: &Path) -> Result<MediaProbe, String> {
        Ok(MediaProbe {
            source_size: Some(PixelSize::new(640, 360)),
            has_audio: true,
            ..Default::default()
        })
    }

    fn single_video_plan(video_is_av1: bool) -> (ExportRequest, ExportPlan) {
        let mut editor = EditorState::new(Language::English, "AV1 input");
        editor.add_media_paths([PathBuf::from("input.mov")]);
        assert!(editor.add_selected_to_timeline());
        let request = request(&editor);
        let plan = ExportPlan::from_request_with_probe(&request, |_| {
            Ok(MediaProbe {
                source_size: Some(PixelSize::new(640, 360)),
                has_audio: true,
                video_is_av1,
            })
        })
        .unwrap();
        (request, plan)
    }

    #[test]
    fn av1_decoder_candidates_are_capability_ordered() {
        let (_, av1_plan) = single_video_plan(true);
        #[cfg(windows)]
        assert_eq!(
            av1_input_decoder_candidates(&av1_plan),
            vec![
                Av1InputDecoder::D3d11va,
                Av1InputDecoder::Dxva2,
                Av1InputDecoder::Cuvid,
                Av1InputDecoder::Qsv,
                Av1InputDecoder::Libaom,
                Av1InputDecoder::Default,
            ]
        );
        #[cfg(target_os = "linux")]
        assert_eq!(
            av1_input_decoder_candidates(&av1_plan),
            vec![
                Av1InputDecoder::Cuvid,
                Av1InputDecoder::Qsv,
                Av1InputDecoder::Libaom,
                Av1InputDecoder::Default,
            ]
        );
        #[cfg(not(any(windows, target_os = "linux")))]
        assert_eq!(
            av1_input_decoder_candidates(&av1_plan),
            vec![Av1InputDecoder::Libaom, Av1InputDecoder::Default]
        );

        let (_, non_av1_plan) = single_video_plan(false);
        assert_eq!(
            av1_input_decoder_candidates(&non_av1_plan),
            vec![Av1InputDecoder::Default]
        );
    }

    #[test]
    fn av1_decoder_input_options_precede_only_av1_input() {
        let (request, plan) = single_video_plan(true);
        let (args, _) = build_ffmpeg_job_with_title_assets_and_input_decoder(
            &request,
            &plan,
            H264Encoder::OpenH264,
            &[],
            Av1InputDecoder::Cuvid,
        )
        .unwrap();
        let input = args.iter().position(|arg| arg == "input.mov").unwrap();
        assert_eq!(&args[input - 3..input], ["-c:v", "av1_cuvid", "-i"]);
    }

    #[test]
    fn libaom_av1_decoder_input_option_is_explicit() {
        assert_eq!(Av1InputDecoder::Libaom.name(), "libaom-av1");
        assert_eq!(Av1InputDecoder::Libaom.input_args(), ["-c:v", "libaom-av1"]);
    }

    #[test]
    fn non_av1_input_arguments_ignore_decoder_candidate() {
        let (request, plan) = single_video_plan(false);
        let (default_args, _) = build_ffmpeg_job(&request, &plan, H264Encoder::OpenH264).unwrap();
        let (candidate_args, _) = build_ffmpeg_job_with_title_assets_and_input_decoder(
            &request,
            &plan,
            H264Encoder::OpenH264,
            &[],
            Av1InputDecoder::Cuvid,
        )
        .unwrap();
        assert_eq!(candidate_args, default_args);
    }

    #[test]
    fn av1_still_input_does_not_expand_retries_or_receive_video_decoder_options() {
        let (request, mut plan) = single_video_plan(true);
        plan.video_tracks[0].clips[0].is_still = true;
        assert_eq!(
            av1_input_decoder_candidates(&plan),
            vec![Av1InputDecoder::Default]
        );
        let (default_args, _) = build_ffmpeg_job_with_title_assets_and_input_decoder(
            &request,
            &plan,
            H264Encoder::OpenH264,
            &[],
            Av1InputDecoder::Default,
        )
        .unwrap();
        let (candidate_args, _) = build_ffmpeg_job_with_title_assets_and_input_decoder(
            &request,
            &plan,
            H264Encoder::OpenH264,
            &[],
            Av1InputDecoder::Cuvid,
        )
        .unwrap();
        assert_eq!(candidate_args, default_args);
    }

    #[test]
    fn export_attempt_failures_identify_decoder_and_encoder() {
        assert_eq!(
            export_attempt_failure(
                Av1InputDecoder::Dxva2,
                H264Encoder::OpenH264,
                "decoder unavailable"
            ),
            "dxva2 input / libopenh264 encoder: decoder unavailable"
        );
    }

    #[cfg(feature = "qualification")]
    #[test]
    fn qualification_delivery_uses_rgba_without_h264_or_audio_mapping() {
        let mut editor = EditorState::new(Language::English, "Qualification format");
        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        assert!(editor.add_selected_to_timeline());
        let request = request(&editor);
        let plan = ExportPlan::from_request_with_probe(&request, probe).unwrap();
        let (args, graph) = build_ffmpeg_job_with_title_assets_and_delivery(
            &request,
            &plan,
            &[],
            VideoDelivery::RawRgbaFrame {
                timeline_tick: Tick(500_000),
            },
            Av1InputDecoder::Default,
        )
        .unwrap();
        assert!(graph.contains("format=rgba,trim=start=0.500000,setpts=PTS-STARTPTS"));
        assert!(!graph.contains("[aout]"));
        assert!(args.windows(2).any(|pair| pair == ["-pix_fmt", "rgba"]));
        assert!(!args.iter().any(|arg| arg == "-c:v"));
    }

    #[cfg(feature = "qualification")]
    #[test]
    fn qualification_tick_must_be_within_export_duration() {
        assert!(validate_qualification_tick(Tick(0), Tick(1)).is_ok());
        assert!(validate_qualification_tick(Tick(-1), Tick(1)).is_err());
        assert!(validate_qualification_tick(Tick(1), Tick(1)).is_err());
    }

    #[test]
    fn export_graph_keeps_exact_frame_rate_for_every_visual_source() {
        let mut editor = EditorState::new(Language::English, "Fractional export");
        editor.add_media_paths(["left.mp4", "right.mp4", "still.png"].map(PathBuf::from));
        let tracks: Vec<_> = editor
            .timeline
            .tracks
            .iter()
            .filter(|track| track.kind == TrackKind::Video)
            .map(|track| track.id)
            .collect();
        let left = editor
            .timeline
            .insert_clip(
                tracks[0],
                MediaId(1),
                Tick(0),
                Tick(2_000_000),
                Tick(1_000_000),
            )
            .unwrap();
        let right = editor
            .timeline
            .insert_clip(
                tracks[0],
                MediaId(2),
                Tick(2_000_000),
                Tick(2_000_000),
                Tick(1_000_000),
            )
            .unwrap();
        editor
            .timeline
            .insert_clip(tracks[1], MediaId(3), Tick(0), Tick(1_000_000), Tick(0))
            .unwrap();
        editor
            .timeline
            .add_video_transition(tracks[0], left, right, Tick(1_000_000), 0.0)
            .unwrap();
        editor
            .timeline
            .add_title(Tick(0), Tick(1_000_000), "日本語")
            .unwrap();
        let mut snapshot = editor.snapshot();
        snapshot.timeline.transitions[0].kind = VideoTransitionKind::DipToBlack;
        for fps in [
            [30, 1],
            [24_000, 1_001],
            [30_000, 1_001],
            [60_000, 1_001],
            [48_000, 2_002],
            [u32::MAX, u32::MAX],
        ] {
            let request = ExportRequest {
                snapshot: snapshot.clone(),
                settings: ProjectSettings {
                    fps,
                    size: [320, 180],
                },
                ..request(&editor)
            };
            let plan = ExportPlan::from_request_with_probe(&request, probe).unwrap();
            let assets: Vec<_> = plan
                .titles
                .iter()
                .map(|planned| TitleAsset {
                    title: planned.title.clone(),
                    path: PathBuf::from("title.tga"),
                })
                .collect();
            let (args, graph) =
                build_ffmpeg_job_with_title_assets(&request, &plan, H264Encoder::OpenH264, &assets)
                    .unwrap();
            let rate = format!("{}/{}", fps[0], fps[1]);
            assert!(
                args.iter()
                    .any(|arg| arg == &format!("color=c=black:s=320x180:r={rate}")),
                "background: {args:?}"
            );
            assert!(
                args.windows(2)
                    .any(|pair| pair == ["-framerate", rate.as_str()]),
                "title input: {args:?}"
            );
            assert_eq!(
                graph.matches(&format!("fps={rate}:round=up,")).count(),
                2,
                "video cadence: {graph}"
            );
            assert!(
                graph.contains(&format!("loop=loop=-1:size=1:start=0,fps={rate},")),
                "still cadence: {graph}"
            );
            assert!(
                graph.contains(&format!("format=rgba,fps={rate},trim=duration=1.000000")),
                "title cadence: {graph}"
            );
            assert!(
                graph.contains(&format!("color=c=black:s=320x180:r={rate},")),
                "matte cadence: {graph}"
            );
        }
    }

    #[test]
    fn real_ffmpeg_export_cadence_retains_exact_rational_time_base() {
        let _guard = real_ffmpeg_test_guard();
        let Some(root) = std::env::var_os("FFMPEG_DIR").map(PathBuf::from) else {
            return;
        };
        let ffmpeg = root.join("bin").join(if cfg!(windows) {
            "ffmpeg.exe"
        } else {
            "ffmpeg"
        });
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let stderr = std::env::temp_dir().join(format!("maelstrom-rational-cadence-{nonce}.log"));
        let _cleanup = TempFiles(vec![stderr.clone()]);
        let mut editor = EditorState::new(Language::English, "Rational cadence");
        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        assert!(editor.add_selected_to_timeline());
        for (fps, reduced) in [
            ([24_000, 1_001], [24_000, 1_001]),
            ([30_000, 1_001], [30_000, 1_001]),
            ([60_000, 1_001], [60_000, 1_001]),
            ([25, 1], [25, 1]),
            ([24_000, 1_007], [24_000, 1_007]),
            ([48_000, 2_002], [24_000, 1_001]),
            ([u32::MAX, u32::MAX], [1, 1]),
        ] {
            let request = ExportRequest {
                settings: ProjectSettings {
                    fps,
                    size: [16, 16],
                },
                ..request(&editor)
            };
            let plan = ExportPlan::from_request_with_probe(&request, probe).unwrap();
            let (args, graph) = build_ffmpeg_job(&request, &plan, H264Encoder::OpenH264).unwrap();
            let color = args
                .iter()
                .find(|arg| arg.starts_with("color=c=black:"))
                .unwrap();
            let cadence = graph
                .split("fps=")
                .nth(1)
                .unwrap()
                .split(',')
                .next()
                .unwrap();
            crate::audio_boundary_tests::run_ffmpeg_bounded(
                &ffmpeg,
                &[
                    "-hide_banner".into(),
                    "-nostdin".into(),
                    "-loglevel".into(),
                    "info".into(),
                    "-f".into(),
                    "lavfi".into(),
                    "-i".into(),
                    color.clone(),
                    "-vf".into(),
                    format!("fps={cadence},showinfo"),
                    "-frames:v".into(),
                    "2".into(),
                    "-f".into(),
                    "null".into(),
                    "-".into(),
                ],
                &stderr,
            );
            let observed = fs::read_to_string(&stderr).unwrap();
            let expected = format!(
                "config in time_base: {}/{}, frame_rate: {}/{}",
                reduced[1], reduced[0], reduced[0], reduced[1]
            );
            assert!(
                observed.contains(&expected),
                "fps={fps:?}: expected {expected}; actual {observed}"
            );
            println!("export cadence fps={fps:?}: {expected}");
        }
    }

    fn brightness_contrast_node(
        enabled: bool,
        brightness: AnimatedScalar,
        contrast: AnimatedScalar,
    ) -> VideoEffectNode {
        VideoEffectNode {
            id: VideoEffectId(1),
            enabled,
            kind: VideoEffectKind::BrightnessContrast(BrightnessContrastEffect {
                brightness,
                contrast,
                ..Default::default()
            }),
        }
    }

    fn vignette_node(id: u32, enabled: bool, amount: f32) -> VideoEffectNode {
        VideoEffectNode {
            id: VideoEffectId(id),
            enabled,
            kind: VideoEffectKind::Vignette(VignetteEffect {
                amount: scalar(amount),
                ..Default::default()
            }),
        }
    }

    fn scalar(value: f32) -> AnimatedScalar {
        AnimatedScalar {
            value,
            keyframes: Vec::new(),
        }
    }

    #[test]
    fn graph_uses_all_four_video_tracks_in_bottom_to_top_order() {
        let mut editor = EditorState::new(Language::English, "Export");
        editor.add_media_paths(["base.mp4", "mid.mp4", "top.mp4", "front.mp4"].map(PathBuf::from));
        let mut tracks = editor
            .timeline
            .tracks
            .iter()
            .filter(|track| track.kind == TrackKind::Video)
            .map(|track| track.id)
            .collect::<Vec<_>>();
        while tracks.len() < 4 {
            tracks.push(editor.timeline.add_track(TrackKind::Video));
        }
        for (track, media) in tracks.into_iter().zip(1..=4) {
            editor
                .timeline
                .insert_clip(track, MediaId(media), Tick(0), Tick(1_000_000), Tick(0))
                .unwrap();
        }
        let request = request(&editor);
        let plan = ExportPlan::from_request_with_probe(&request, probe).unwrap();
        assert_eq!(
            plan.video_tracks
                .iter()
                .map(|track| track.clips[0].clip.media.0)
                .collect::<Vec<_>>(),
            [1, 2, 3, 4]
        );
        let (_, graph) = build_ffmpeg_job(&request, &plan, H264Encoder::OpenH264).unwrap();
        assert!(graph.contains("[vbase0][vl0]overlay"));
        assert!(graph.contains("[vbase3][vl3]overlay"));
    }

    #[test]
    fn graph_lowers_compositor_transform_and_video_envelope() {
        let mut editor = EditorState::new(Language::English, "Export");
        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        assert!(editor.add_selected_to_timeline());
        let mut snapshot = editor.snapshot();
        let clip = &mut snapshot
            .timeline
            .tracks
            .iter_mut()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap()
            .clips[0];
        clip.transform = ClipTransform {
            opacity: 0.5,
            scale_x: 0.5,
            scale_y: 1.25,
            rotation_degrees: 90.0,
            crop_left: 0.1,
            crop_right: 0.2,
            flip_h: true,
            ..Default::default()
        };
        clip.fade_in.duration = Tick(500_000);
        clip.fade_out.duration = Tick(250_000);
        let request = ExportRequest {
            snapshot,
            ..request(&editor)
        };
        let plan = ExportPlan::from_request_with_probe(&request, probe).unwrap();
        let (_, graph) = build_ffmpeg_job(&request, &plan, H264Encoder::OpenH264).unwrap();
        assert!(graph.contains("crop=w=448.000"));
        assert!(graph.contains(",hflip"));
        assert!(graph.contains("format=rgba,premultiply=inplace=1,crop="));
        assert!(graph.contains(",hflip,unpremultiply=inplace=1"));
        assert!(graph.contains("colorchannelmixer=aa=0.500000"));
        assert!(graph.contains(",premultiply=inplace=1,rotate=1.570796"));
        assert!(graph.contains("alpha=straight:eof_action=pass"));
        assert!(graph.contains("rotate=1.570796"));
        assert!(graph.contains("geq=r='r(X\\,Y)'"));
        assert!(graph.contains("pow("));
    }

    #[test]
    fn graph_clamps_extreme_basic_correction_before_endpoint_powers() {
        let mut editor = EditorState::new(Language::English, "Color export");
        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        assert!(editor.add_selected_to_timeline());
        let mut snapshot = editor.snapshot();
        let clip = &mut snapshot
            .timeline
            .tracks
            .iter_mut()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap()
            .clips[0];
        clip.video_effects
            .push(brightness_contrast_node(true, scalar(-1.0), scalar(4.0)))
            .unwrap();
        assert!(
            clip.video_effects
                .edit(0, |node| {
                    let VideoEffectKind::BrightnessContrast(effect) = &mut node.kind else {
                        panic!("expected the basic correction operation");
                    };
                    effect.temperature = scalar(0.4);
                    effect.tint = scalar(-0.2);
                    effect.saturation = scalar(1.25);
                    effect.exposure = scalar(0.5);
                    effect.highlights = scalar(0.3);
                    effect.shadows = scalar(-0.4);
                    effect.whites = scalar(0.55);
                    effect.blacks = scalar(-0.65);
                })
                .is_ok()
        );
        let request = ExportRequest {
            snapshot,
            ..request(&editor)
        };
        let plan = ExportPlan::from_request_with_probe(&request, probe).unwrap();
        let (_, graph) = build_ffmpeg_job(&request, &plan, H264Encoder::OpenH264).unwrap();
        assert!(graph.contains("r(X\\,Y)/255+0.10*(0.400000)+0.05*(-0.200000)"));
        assert!(graph.contains("g(X\\,Y)/255+-0.05*(-0.200000)"));
        assert!(graph.contains("b(X\\,Y)/255+-0.10*(0.400000)+0.05*(-0.200000)"));
        assert!(graph.contains("pow(2\\,(0.500000))"));
        assert!(graph.contains("0.2126*"));
        assert!(graph.contains("*(1.250000)"));
        assert!(graph.contains("*(4.000000)+0.5+(-1.000000)"));
        assert!(graph.contains("0.25*(0.300000)"));
        assert!(graph.contains("0.25*(-0.400000)"));
        assert!(graph.contains("0.20*(0.550000)*pow(ld(4)\\,8)"));
        assert!(graph.contains("0.20*(-0.650000)*pow(1-ld(4)\\,8)"));
        assert!(graph.contains("st(4\\,max(0\\,min(1\\,"));
        assert!(graph.contains(":a='alpha(X\\,Y)'"));
        assert!(!graph.contains(",curves=interp=natural:"));
    }

    #[test]
    fn graph_lowers_non_identity_rgb_curves_after_basic_correction() {
        let mut editor = EditorState::new(Language::English, "Curve export");
        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        assert!(editor.add_selected_to_timeline());
        let mut snapshot = editor.snapshot();
        let clip = &mut snapshot
            .timeline
            .tracks
            .iter_mut()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap()
            .clips[0];
        clip.video_effects
            .push(brightness_contrast_node(true, scalar(0.0), scalar(1.0)))
            .unwrap();
        assert!(
            clip.video_effects
                .edit(0, |node| {
                    let VideoEffectKind::BrightnessContrast(effect) = &mut node.kind else {
                        panic!("expected the basic correction operation");
                    };
                    effect.curves.master = ColorCurve {
                        points: vec![
                            CurvePoint { x: 0.0, y: 0.0 },
                            CurvePoint { x: 0.5, y: 0.75 },
                            CurvePoint { x: 1.0, y: 1.0 },
                        ],
                    };
                })
                .is_ok()
        );
        let request = ExportRequest {
            snapshot,
            ..request(&editor)
        };
        let plan = ExportPlan::from_request_with_probe(&request, probe).unwrap();
        let (_, graph) = build_ffmpeg_job(&request, &plan, H264Encoder::OpenH264).unwrap();
        let geq = graph.find(",geq=r=").unwrap();
        let curves = graph.find(",curves=interp=natural:").unwrap();
        assert!(geq < curves, "curves must follow basic correction: {graph}");
        assert!(graph.contains("master='0/0 0.5/0.75 1/1'"), "{graph}");
        assert!(graph.contains("red='0/0 1/1'"), "{graph}");
        assert!(graph.contains("green='0/0 1/1'"), "{graph}");
        assert!(graph.contains("blue='0/0 1/1'"), "{graph}");
    }

    #[test]
    fn graph_lowers_source_time_linear_and_hold_keyframes() {
        let mut editor = EditorState::new(Language::English, "Animated color export");
        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        assert!(editor.add_selected_to_timeline());
        let mut snapshot = editor.snapshot();
        let clip = &mut snapshot
            .timeline
            .tracks
            .iter_mut()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap()
            .clips[0];
        clip.source_in = Tick(1_000_000);
        clip.video_effects
            .push(brightness_contrast_node(
                true,
                AnimatedScalar {
                    value: 0.0,
                    keyframes: vec![
                        ScalarKeyframe {
                            source_tick: Tick(1_000_000),
                            value: 0.0,
                            interpolation: KeyframeInterpolation::Linear,
                        },
                        ScalarKeyframe {
                            source_tick: Tick(2_000_000),
                            value: 0.5,
                            interpolation: KeyframeInterpolation::Hold,
                        },
                        ScalarKeyframe {
                            source_tick: Tick(3_000_000),
                            value: -0.25,
                            interpolation: KeyframeInterpolation::Linear,
                        },
                    ],
                },
                scalar(1.0),
            ))
            .unwrap();
        assert!(
            clip.video_effects
                .edit(0, |node| {
                    let VideoEffectKind::BrightnessContrast(effect) = &mut node.kind else {
                        panic!("expected the basic correction operation");
                    };
                    effect.temperature = AnimatedScalar {
                        value: 0.0,
                        keyframes: vec![
                            ScalarKeyframe {
                                source_tick: Tick(1_000_000),
                                value: -0.5,
                                interpolation: KeyframeInterpolation::Linear,
                            },
                            ScalarKeyframe {
                                source_tick: Tick(2_000_000),
                                value: 0.5,
                                interpolation: KeyframeInterpolation::Linear,
                            },
                        ],
                    };
                    effect.whites = AnimatedScalar {
                        value: 0.0,
                        keyframes: vec![
                            ScalarKeyframe {
                                source_tick: Tick(1_000_000),
                                value: -0.25,
                                interpolation: KeyframeInterpolation::Linear,
                            },
                            ScalarKeyframe {
                                source_tick: Tick(2_000_000),
                                value: 0.75,
                                interpolation: KeyframeInterpolation::Linear,
                            },
                        ],
                    };
                })
                .is_ok()
        );
        let request = ExportRequest {
            snapshot,
            ..request(&editor)
        };
        let plan = ExportPlan::from_request_with_probe(&request, probe).unwrap();
        let (_, graph) = build_ffmpeg_job(&request, &plan, H264Encoder::OpenH264).unwrap();
        assert!(graph.contains("(T+1.000000)"), "{graph}");
        assert!(graph.contains(
            "0.000000+(0.500000-0.000000)*(((T+1.000000)-1.000000)/(2.000000-1.000000))"
        ));
        assert!(graph.contains(
            "-0.500000+(0.500000--0.500000)*(((T+1.000000)-1.000000)/(2.000000-1.000000))"
        ));
        assert!(graph.contains(
            "-0.250000+(0.750000--0.250000)*(((T+1.000000)-1.000000)/(2.000000-1.000000))"
        ));
        // The second key holds its value through the interval leading to the third key.
        assert!(
            graph.contains("if(lt((T+1.000000)\\,3.000000)\\,0.500000\\,-0.250000)"),
            "{graph}"
        );
    }

    #[test]
    fn animated_scalar_expression_lowers_the_timeline_easing_curves() {
        let expression = animated_scalar_expression(
            &AnimatedScalar {
                value: 0.0,
                keyframes: vec![
                    ScalarKeyframe {
                        source_tick: Tick(0),
                        value: 0.0,
                        interpolation: KeyframeInterpolation::Smooth,
                    },
                    ScalarKeyframe {
                        source_tick: Tick(1_000_000),
                        value: 1.0,
                        interpolation: KeyframeInterpolation::EaseIn,
                    },
                    ScalarKeyframe {
                        source_tick: Tick(2_000_000),
                        value: 2.0,
                        interpolation: KeyframeInterpolation::EaseOut,
                    },
                    ScalarKeyframe {
                        source_tick: Tick(3_000_000),
                        value: 3.0,
                        interpolation: KeyframeInterpolation::Linear,
                    },
                ],
            },
            Tick(0),
            "T",
        );

        let normalized = "((T+0.000000)-0.000000)/(1.000000-0.000000)";
        assert!(
            expression.contains(&format!(
                "({normalized})*({normalized})*(3-2*({normalized}))"
            )),
            "{expression}"
        );
        let normalized = "((T+0.000000)-1.000000)/(2.000000-1.000000)";
        assert!(
            expression.contains(&format!("({normalized})*({normalized})")),
            "{expression}"
        );
        let normalized = "((T+0.000000)-2.000000)/(3.000000-2.000000)";
        assert!(
            expression.contains(&format!("1-(1-({normalized}))*(1-({normalized}))")),
            "{expression}"
        );
    }

    #[test]
    fn disabled_brightness_contrast_is_a_neutral_export_bypass() {
        let mut editor = EditorState::new(Language::English, "Color bypass");
        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        assert!(editor.add_selected_to_timeline());
        let mut snapshot = editor.snapshot();
        snapshot
            .timeline
            .tracks
            .iter_mut()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap()
            .clips[0]
            .video_effects
            .push(brightness_contrast_node(false, scalar(1.0), scalar(4.0)))
            .unwrap();
        let request = ExportRequest {
            snapshot,
            ..request(&editor)
        };
        let plan = ExportPlan::from_request_with_probe(&request, probe).unwrap();
        let (_, graph) = build_ffmpeg_job(&request, &plan, H264Encoder::OpenH264).unwrap();
        assert!(!graph.contains("r(X\\,Y)"), "{graph}");
    }

    #[test]
    fn export_plan_reports_invalid_video_effect_graphs_before_filter_lowering() {
        let mut editor = EditorState::new(Language::English, "Invalid color graph");
        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        assert!(editor.add_selected_to_timeline());
        let mut snapshot = editor.snapshot();
        let clip = &mut snapshot
            .timeline
            .tracks
            .iter_mut()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap()
            .clips[0];
        clip.video_effects = vec![brightness_contrast_node(
            true,
            scalar(f32::NAN),
            scalar(1.0),
        )]
        .into();
        let request = ExportRequest {
            snapshot,
            ..request(&editor)
        };

        let error = ExportPlan::from_request_with_probe(&request, probe).unwrap_err();
        assert!(error.contains("clip"), "{error}");
        assert!(error.contains("invalid video effect graph"), "{error}");
    }

    #[test]
    fn color_stack_lowers_enabled_nodes_in_order_and_skips_disabled_nodes() {
        let mut editor = EditorState::new(Language::English, "Ordered color stack");
        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        assert!(editor.add_selected_to_timeline());
        let mut snapshot = editor.snapshot();
        let clip = &mut snapshot
            .timeline
            .tracks
            .iter_mut()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap()
            .clips[0];
        let mut middle = brightness_contrast_node(false, scalar(0.4), scalar(3.0));
        middle.id = VideoEffectId(2);
        let mut last = brightness_contrast_node(true, scalar(-0.5), scalar(1.0));
        last.id = VideoEffectId(3);
        clip.video_effects = vec![
            brightness_contrast_node(true, scalar(0.8), scalar(1.0)),
            middle,
            last,
        ]
        .into();
        let request = ExportRequest {
            snapshot,
            ..request(&editor)
        };
        let plan = ExportPlan::from_request_with_probe(&request, probe).unwrap();
        let (_, graph) = build_ffmpeg_job(&request, &plan, H264Encoder::OpenH264).unwrap();

        assert_eq!(graph.matches(",geq=r=").count(), 2, "{graph}");
        let first = graph.find("+0.5+(0.800000)").expect("first stack node");
        let second = graph.find("+0.5+(-0.500000)").expect("second stack node");
        assert!(first < second, "color stack order changed: {graph}");
        assert!(
            !graph.contains("3.000000"),
            "disabled node reached export: {graph}"
        );
    }

    #[test]
    fn full_ten_node_color_stack_compiles_once_and_lowers_every_node() {
        let mut editor = EditorState::new(Language::English, "Ten-node color stack");
        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        assert!(editor.add_selected_to_timeline());
        let mut snapshot = editor.snapshot();
        let clip = &mut snapshot
            .timeline
            .tracks
            .iter_mut()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap()
            .clips[0];
        clip.video_effects = (0..nle_timeline::MAX_VIDEO_EFFECTS_PER_CLIP)
            .map(|index| {
                let mut node =
                    brightness_contrast_node(true, scalar(0.01 * (index + 1) as f32), scalar(1.0));
                node.id = VideoEffectId(index as u32 + 1);
                node
            })
            .collect::<Vec<_>>()
            .into();
        let request = ExportRequest {
            snapshot,
            ..request(&editor)
        };

        let plan = ExportPlan::from_request_with_probe(&request, probe).unwrap();
        assert_eq!(plan.video_tracks[0].clips[0].effects.len(), 10);
        let (_, graph) = build_ffmpeg_job(&request, &plan, H264Encoder::OpenH264).unwrap();
        assert_eq!(graph.matches(",geq=r=").count(), 10, "{graph}");
    }

    #[test]
    fn color_correction_precedes_opacity_fade_and_rotation() {
        let mut editor = EditorState::new(Language::English, "Color fade order");
        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        assert!(editor.add_selected_to_timeline());
        let mut snapshot = editor.snapshot();
        let clip = &mut snapshot
            .timeline
            .tracks
            .iter_mut()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap()
            .clips[0];
        clip.fade_in.duration = Tick(100_000);
        clip.transform.rotation_degrees = 10.0;
        clip.video_effects
            .push(brightness_contrast_node(true, scalar(0.1), scalar(1.0)))
            .unwrap();
        let request = ExportRequest {
            snapshot,
            ..request(&editor)
        };
        let plan = ExportPlan::from_request_with_probe(&request, probe).unwrap();
        let (_, graph) = build_ffmpeg_job(&request, &plan, H264Encoder::OpenH264).unwrap();
        assert_eq!(graph.matches("geq=r='").count(), 2, "{graph}");
        let color = graph.find("max(0\\,min(255").unwrap();
        let opacity = graph.find("colorchannelmixer=aa=").unwrap();
        let fade = graph.rfind("geq=r='").unwrap();
        let rotate = graph.find("rotate=").unwrap();
        assert!(
            color < opacity && opacity < fade && fade < rotate,
            "{graph}"
        );
        assert!(graph.contains("a='alpha(X\\,Y)*if(lt(T\\,0.100000)"));
    }

    #[test]
    fn delayed_video_keeps_its_timeline_pts_before_overlay() {
        let mut editor = EditorState::new(Language::English, "Export");
        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        assert!(editor.insert_media_at(1, Tick(3_000_000)));
        let request = request(&editor);
        let plan = ExportPlan::from_request_with_probe(&request, probe).unwrap();
        let (_, graph) = build_ffmpeg_job(&request, &plan, H264Encoder::OpenH264).unwrap();
        assert!(graph.contains("setpts=PTS+3.000000/TB"));
        assert!(!graph.contains("rotate=0.000000000"));
        assert!(graph.contains(
            "overlay=x='960.000-overlay_w/2':y='540.000-overlay_h/2':eval=frame:format=rgb"
        ));
        assert!(graph.contains("between(t,3.000000,18.000000)"));
    }

    #[test]
    fn portrait_project_uses_shared_fit_geometry() {
        let mut editor = EditorState::new(Language::English, "Portrait export");
        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        assert!(editor.add_selected_to_timeline());
        let request = ExportRequest {
            settings: ProjectSettings {
                fps: [30, 1],
                size: [1080, 1920],
            },
            ..request(&editor)
        };
        let plan = ExportPlan::from_request_with_probe(&request, probe).unwrap();
        let (_, graph) = build_ffmpeg_job(&request, &plan, H264Encoder::OpenH264).unwrap();
        assert!(graph.contains("scale=1080:608"), "{graph}");
        assert!(
            graph.contains("overlay=x='540.000-overlay_w/2':y='960.000-overlay_h/2'"),
            "{graph}"
        );
    }

    #[test]
    fn muted_late_track_preserves_full_sequence_duration_as_black() {
        let mut editor = EditorState::new(Language::English, "Muted tail");
        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        let mut tracks = editor
            .timeline
            .tracks
            .iter()
            .filter(|track| track.kind == TrackKind::Video)
            .map(|track| track.id)
            .take(2)
            .collect::<Vec<_>>();
        if tracks.len() < 2 {
            tracks.push(editor.timeline.add_track(TrackKind::Video));
        }
        editor
            .timeline
            .insert_clip(tracks[0], MediaId(1), Tick(0), Tick(1_000_000), Tick(0))
            .unwrap();
        editor
            .timeline
            .insert_clip(
                tracks[1],
                MediaId(1),
                Tick(5_000_000),
                Tick(1_000_000),
                Tick(0),
            )
            .unwrap();
        editor.timeline.set_track_muted(tracks[1], true).unwrap();
        let request = request(&editor);
        let plan = ExportPlan::from_request_with_probe(&request, probe).unwrap();
        assert_eq!(plan.duration, Tick(6_000_000));
        assert_eq!(plan.video_tracks.len(), 1);
    }

    #[test]
    fn still_image_plan_loops_without_source_seek_and_is_graph_trimmed() {
        let mut editor = EditorState::new(Language::English, "Still export");
        editor.add_media_paths([PathBuf::from("still.png")]);
        let track = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap()
            .id;
        editor
            .timeline
            .insert_clip(
                track,
                MediaId(1),
                Tick(2_000_000),
                Tick(1_500_000),
                Tick(900_000),
            )
            .unwrap();
        let request = request(&editor);
        let plan = ExportPlan::from_request_with_probe(&request, probe).unwrap();
        let still = &plan.video_tracks[0].clips[0];
        assert!(still.is_still);
        assert_eq!(still.source_size, PixelSize::new(640, 360));

        let (args, graph) = build_ffmpeg_job(&request, &plan, H264Encoder::OpenH264).unwrap();
        let input = args
            .iter()
            .position(|arg| arg == "still.png")
            .expect("still image input");
        assert_eq!(&args[input - 3..input], ["-t", "1.500000", "-i"]);
        assert!(
            !args.windows(2).any(|pair| pair == ["-ss", "0.900000"]),
            "still input must not seek its single source frame: {args:?}"
        );
        assert!(
            graph.contains(
                "select='eq(n\\,0)',loop=loop=-1:size=1:start=0,fps=30/1,trim=duration=1.500000,format=rgba,premultiply=inplace=1,crop="
            ),
            "{graph}"
        );
        assert!(graph.contains("setpts=PTS+2.000000/TB"), "{graph}");
        assert!(graph.contains("between(t,2.000000,3.500000)"), "{graph}");
        assert!(
            args.windows(3)
                .any(|window| window == ["-t", "3.500000", "-progress"]),
            "muxed output must be capped to the immutable plan duration: {args:?}"
        );
    }

    #[test]
    fn cross_dissolve_expands_video_ranges_and_uses_raw_quadratic_opacity() {
        let mut editor = EditorState::new(Language::English, "Transition export");
        editor.add_media_paths([PathBuf::from("left.mp4"), PathBuf::from("right.mp4")]);
        let track = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap()
            .id;
        let left = editor
            .timeline
            .insert_clip(track, MediaId(1), Tick(0), Tick(2_000_000), Tick(1_000_000))
            .unwrap();
        let right = editor
            .timeline
            .insert_clip(
                track,
                MediaId(2),
                Tick(2_000_000),
                Tick(2_000_000),
                Tick(1_000_000),
            )
            .unwrap();
        editor
            .timeline
            .add_video_transition(track, left, right, Tick(1_000_000), 0.0)
            .unwrap();
        let mut snapshot = editor.snapshot();
        for media in &mut snapshot.media {
            media.duration = Some(Tick(5_000_000));
        }
        snapshot
            .timeline
            .tracks
            .iter_mut()
            .find(|item| item.id == track)
            .unwrap()
            .clips
            .iter_mut()
            .find(|item| item.id == right)
            .unwrap()
            .fade_in = Fade {
            duration: Tick(250_000),
            curve: 0.0,
        };
        let request = ExportRequest {
            snapshot,
            ..request(&editor)
        };
        let plan = ExportPlan::from_request_with_probe(&request, probe).unwrap();
        let clips = &plan.video_tracks[0].clips;
        assert_eq!(clips[0].input_source_in, Tick(1_000_000));
        assert_eq!(clips[0].input_duration, Tick(2_500_000));
        assert_eq!(clips[0].timeline_start, Tick(0));
        assert_eq!(clips[0].timeline_end, Tick(2_500_000));
        assert_eq!(clips[1].input_source_in, Tick(500_000));
        assert_eq!(clips[1].input_duration, Tick(2_500_000));
        assert_eq!(clips[1].timeline_start, Tick(1_500_000));
        assert_eq!(clips[1].timeline_end, Tick(4_000_000));

        let (args, graph) = build_ffmpeg_job(&request, &plan, H264Encoder::OpenH264).unwrap();
        let left_input = args.iter().position(|arg| arg == "left.mp4").unwrap();
        assert_eq!(
            &args[left_input - 4..left_input],
            ["-ss", "1.000000", "-noaccurate_seek", "-i"]
        );
        let right_input = args.iter().position(|arg| arg == "right.mp4").unwrap();
        assert_eq!(
            &args[right_input - 4..right_input],
            ["-ss", "0.500000", "-noaccurate_seek", "-i"]
        );
        assert!(
            graph.contains(
                "fps=30/1:round=up,trim=start=0:duration=2.500000,setpts=PTS-STARTPTS,format=rgba,premultiply=inplace=1,crop="
            ),
            "{graph}"
        );
        assert!(graph.contains("between(t,0.000000,2.500000)"), "{graph}");
        assert!(graph.contains("between(t,1.500000,4.000000)"), "{graph}");
        assert!(
            graph.contains("2*0.500000*(T/1.000000)+(1-2*0.500000)*pow((T/1.000000)\\,2)"),
            "{graph}"
        );
        assert!(!graph.contains(",1.500000)"), "{graph}");
        assert!(graph.contains("(T-0.500000)"), "{graph}");
    }

    #[test]
    fn dip_to_black_uses_normal_ranges_and_a_track_depth_matte() {
        let mut editor = EditorState::new(Language::English, "Dip export");
        editor.add_media_paths([PathBuf::from("left.mp4"), PathBuf::from("right.mp4")]);
        let track = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap()
            .id;
        let left = editor
            .timeline
            .insert_clip(track, MediaId(1), Tick(0), Tick(2_000_000), Tick(1_000_000))
            .unwrap();
        let right = editor
            .timeline
            .insert_clip(
                track,
                MediaId(2),
                Tick(2_000_000),
                Tick(2_000_000),
                Tick(1_000_000),
            )
            .unwrap();
        editor
            .timeline
            .add_video_transition(track, left, right, Tick(1_000_000), 0.0)
            .unwrap();
        let mut snapshot = editor.snapshot();
        snapshot.timeline.transitions[0].kind = VideoTransitionKind::DipToBlack;
        let request = ExportRequest {
            snapshot,
            ..request(&editor)
        };
        let plan = ExportPlan::from_request_with_probe(&request, probe).unwrap();
        let clips = &plan.video_tracks[0].clips;
        assert_eq!(clips[0].input_source_in, Tick(1_000_000));
        assert_eq!(clips[0].input_duration, Tick(2_000_000));
        assert_eq!(clips[0].timeline_end, Tick(2_000_000));
        assert_eq!(clips[1].input_source_in, Tick(1_000_000));
        assert_eq!(clips[1].input_duration, Tick(2_000_000));
        assert_eq!(clips[1].timeline_start, Tick(2_000_000));
        let matte = clips[0].outgoing_matte.as_ref().unwrap();
        assert_eq!(matte.start, Tick(1_500_000));
        assert_eq!(matte.end, Tick(2_500_000));

        let (args, graph) = build_ffmpeg_job(&request, &plan, H264Encoder::OpenH264).unwrap();
        let left_input = args.iter().position(|arg| arg == "left.mp4").unwrap();
        assert_eq!(
            &args[left_input - 4..left_input],
            ["-ss", "1.000000", "-noaccurate_seek", "-i"]
        );
        assert!(
            graph.contains("color=c=black:s=1920x1080:r=30/1"),
            "{graph}"
        );
        assert!(
            graph.contains("[vbase1][vdip0]overlay=format=rgb:alpha=straight"),
            "{graph}"
        );
        assert!(
            graph.contains("2*0.500000*(T/0.500000)+(1-2*0.500000)*pow((T/0.500000)\\,2)"),
            "{graph}"
        );
        assert!(graph.contains("1-(if(gt(T\\,0.000000)"), "{graph}");
        assert!(!graph.contains("between(t,1.500000,4.000000)"), "{graph}");
    }

    #[test]
    fn every_video_transition_kind_lowers_to_a_distinct_export_behavior() {
        let expected = [
            (VideoTransitionKind::CrossDissolve, "alpha(X\\,Y)*", None),
            (VideoTransitionKind::FilmDissolve, "pow((if(", None),
            (VideoTransitionKind::DipToBlack, "color=c=black", None),
            (VideoTransitionKind::DipToWhite, "color=c=white", None),
            (VideoTransitionKind::WipeLeft, "if(lte((X+0.5)/W\\,", None),
            (VideoTransitionKind::WipeRight, "if(gte((X+0.5)/W\\,", None),
            (VideoTransitionKind::WipeUp, "if(lte((Y+0.5)/H\\,", None),
            (VideoTransitionKind::WipeDown, "if(gte((Y+0.5)/H\\,", None),
            (
                VideoTransitionKind::SlideFromLeft,
                "overlay_w*(1-(if(",
                None,
            ),
            (
                VideoTransitionKind::SlideFromRight,
                "overlay_w*(1-(if(",
                None,
            ),
            (VideoTransitionKind::SlideFromTop, "overlay_h*(1-(if(", None),
            (
                VideoTransitionKind::SlideFromBottom,
                "overlay_h*(1-(if(",
                None,
            ),
            (
                VideoTransitionKind::PushFromLeft,
                "overlay_w*((if(",
                Some("overlay_w*(if("),
            ),
            (
                VideoTransitionKind::PushFromRight,
                "overlay_w*(1-(if(",
                Some("-overlay_w*(if("),
            ),
            (
                VideoTransitionKind::PushFromTop,
                "overlay_h*((if(",
                Some("overlay_h*(if("),
            ),
            (
                VideoTransitionKind::PushFromBottom,
                "overlay_h*(1-(if(",
                Some("-overlay_h*(if("),
            ),
        ];

        for (kind, marker, outgoing_marker) in expected {
            let mut editor = EditorState::new(Language::English, "Transition kinds");
            editor.add_media_paths([PathBuf::from("left.mp4"), PathBuf::from("right.mp4")]);
            let track = editor
                .timeline
                .tracks
                .iter()
                .find(|track| track.kind == TrackKind::Video)
                .unwrap()
                .id;
            let left = editor
                .timeline
                .insert_clip(track, MediaId(1), Tick(0), Tick(2_000_000), Tick(1_000_000))
                .unwrap();
            let right = editor
                .timeline
                .insert_clip(
                    track,
                    MediaId(2),
                    Tick(2_000_000),
                    Tick(2_000_000),
                    Tick(1_000_000),
                )
                .unwrap();
            editor
                .timeline
                .add_video_transition_of_kind(track, left, right, Tick(1_000_000), 0.5, kind)
                .unwrap();
            let mut snapshot = editor.snapshot();
            for media in &mut snapshot.media {
                media.duration = Some(Tick(5_000_000));
            }
            let request = ExportRequest {
                snapshot,
                ..request(&editor)
            };
            let plan = ExportPlan::from_request_with_probe(&request, probe).unwrap();
            let (_, graph) = build_ffmpeg_job(&request, &plan, H264Encoder::OpenH264).unwrap();
            assert!(graph.contains(marker), "{kind:?}: {graph}");
            if let Some(outgoing_marker) = outgoing_marker {
                assert!(graph.contains(outgoing_marker), "{kind:?}: {graph}");
            }
            assert!(graph.contains("2*0.750000"), "{kind:?}: {graph}");
            if matches!(kind, VideoTransitionKind::FilmDissolve) {
                assert!(graph.contains("0.650000"), "{graph}");
            }
            if matches!(
                kind,
                VideoTransitionKind::SlideFromRight | VideoTransitionKind::SlideFromBottom
            ) {
                assert!(graph.contains("+overlay_"), "{kind:?}: {graph}");
            }
            if matches!(
                kind,
                VideoTransitionKind::SlideFromLeft | VideoTransitionKind::SlideFromTop
            ) {
                assert!(graph.contains("-overlay_"), "{kind:?}: {graph}");
            }
        }
    }

    #[test]
    fn mixed_adjacent_transitions_keep_roles_and_check_centered_windows() {
        let mut editor = EditorState::new(Language::English, "Mixed transitions");
        editor.add_media_paths([
            PathBuf::from("left.mp4"),
            PathBuf::from("middle.mp4"),
            PathBuf::from("right.mp4"),
        ]);
        let track = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap()
            .id;
        let left = editor
            .timeline
            .insert_clip(track, MediaId(1), Tick(0), Tick(2_000_000), Tick(1_000_000))
            .unwrap();
        let middle = editor
            .timeline
            .insert_clip(
                track,
                MediaId(2),
                Tick(2_000_000),
                Tick(1_000_001),
                Tick(1_000_000),
            )
            .unwrap();
        let right = editor
            .timeline
            .insert_clip(
                track,
                MediaId(3),
                Tick(3_000_001),
                Tick(2_000_000),
                Tick(1_000_000),
            )
            .unwrap();
        editor
            .timeline
            .add_video_transition(track, left, middle, Tick(1_000_000), 0.0)
            .unwrap();
        editor
            .timeline
            .add_video_transition(track, middle, right, Tick(1_000_000), 0.0)
            .unwrap();
        let mut snapshot = editor.snapshot();
        snapshot.timeline.transitions[0].kind = VideoTransitionKind::DipToBlack;
        for transition in &mut snapshot.timeline.transitions {
            transition.duration = Tick(1_000_001);
        }
        for media in &mut snapshot.media {
            media.duration = Some(Tick(5_000_000));
        }
        let request = ExportRequest {
            snapshot,
            ..request(&editor)
        };
        let plan = ExportPlan::from_request_with_probe(&request, probe).unwrap();
        let middle = &plan.video_tracks[0].clips[1];
        assert_eq!(middle.input_source_in, Tick(1_000_000));
        assert_eq!(middle.input_duration, Tick(1_500_002));
        assert!(matches!(
            middle.incoming_opacity,
            Some(TransitionOpacity::IncomingDip(_))
        ));
        assert!(middle.outgoing_matte.is_none());
    }

    #[test]
    fn incoming_slide_and_outgoing_push_both_lower_for_the_middle_clip() {
        let mut editor = EditorState::new(Language::English, "Slide then Push");
        editor.add_media_paths([
            PathBuf::from("left.mp4"),
            PathBuf::from("middle.mp4"),
            PathBuf::from("right.mp4"),
        ]);
        let track = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap()
            .id;
        let left = editor
            .timeline
            .insert_clip(track, MediaId(1), Tick(0), Tick(2_000_000), Tick(1_000_000))
            .unwrap();
        let middle = editor
            .timeline
            .insert_clip(
                track,
                MediaId(2),
                Tick(2_000_000),
                Tick(2_000_000),
                Tick(1_000_000),
            )
            .unwrap();
        let right = editor
            .timeline
            .insert_clip(
                track,
                MediaId(3),
                Tick(4_000_000),
                Tick(2_000_000),
                Tick(1_000_000),
            )
            .unwrap();
        editor
            .timeline
            .add_video_transition_of_kind(
                track,
                left,
                middle,
                Tick(1_000_000),
                0.0,
                VideoTransitionKind::SlideFromLeft,
            )
            .unwrap();
        editor
            .timeline
            .add_video_transition_of_kind(
                track,
                middle,
                right,
                Tick(1_000_000),
                0.0,
                VideoTransitionKind::PushFromLeft,
            )
            .unwrap();
        let mut snapshot = editor.snapshot();
        for media in &mut snapshot.media {
            media.duration = Some(Tick(5_000_000));
        }
        let request = ExportRequest {
            snapshot,
            ..request(&editor)
        };
        let plan = ExportPlan::from_request_with_probe(&request, probe).unwrap();
        let middle = &plan.video_tracks[0].clips[1];
        assert!(middle.incoming_slide.is_some());
        assert!(middle.outgoing_push.is_some());

        let (center_x, center_y) = quad_center(middle.quad);
        let (x, y) = transition_overlay_position(middle, center_x, center_y);
        assert!(x.contains("-overlay_w*(1-"), "incoming Slide missing: {x}");
        assert!(x.contains("+overlay_w*("), "outgoing Push missing: {x}");
        assert!(x.contains("(t-1.500000)"), "incoming timing missing: {x}");
        assert!(x.contains("(t-3.500000)"), "outgoing timing missing: {x}");
        assert_eq!(y, "540.000-overlay_h/2");

        let (_, graph) = build_ffmpeg_job(&request, &plan, H264Encoder::OpenH264).unwrap();
        assert!(graph.contains(&format!("x='{x}'")), "{graph}");
    }

    #[test]
    fn adjacent_cross_dissolves_merge_middle_clip_ranges() {
        let mut editor = EditorState::new(Language::English, "Adjacent transitions");
        editor.add_media_paths([
            PathBuf::from("left.mp4"),
            PathBuf::from("middle.mp4"),
            PathBuf::from("right.mp4"),
        ]);
        let track = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap()
            .id;
        let left = editor
            .timeline
            .insert_clip(track, MediaId(1), Tick(0), Tick(2_000_000), Tick(1_000_000))
            .unwrap();
        let middle = editor
            .timeline
            .insert_clip(
                track,
                MediaId(2),
                Tick(2_000_000),
                Tick(2_000_000),
                Tick(1_000_000),
            )
            .unwrap();
        let right = editor
            .timeline
            .insert_clip(
                track,
                MediaId(3),
                Tick(4_000_000),
                Tick(2_000_000),
                Tick(1_000_000),
            )
            .unwrap();
        editor
            .timeline
            .add_video_transition(track, left, middle, Tick(1_000_000), 0.0)
            .unwrap();
        editor
            .timeline
            .add_video_transition(track, middle, right, Tick(1_000_000), 0.0)
            .unwrap();
        let mut snapshot = editor.snapshot();
        for media in &mut snapshot.media {
            media.duration = Some(Tick(5_000_000));
        }
        let request = ExportRequest {
            snapshot,
            ..request(&editor)
        };
        let plan = ExportPlan::from_request_with_probe(&request, probe).unwrap();
        let middle = &plan.video_tracks[0].clips[1];
        assert_eq!(middle.input_source_in, Tick(500_000));
        assert_eq!(middle.input_duration, Tick(3_000_000));
        assert_eq!(middle.timeline_start, Tick(1_500_000));
        assert_eq!(middle.timeline_end, Tick(4_500_000));
        assert!(matches!(
            middle.incoming_opacity,
            Some(TransitionOpacity::IncomingCross(_))
        ));
    }

    #[test]
    fn export_rejects_raw_overlapping_transition_windows() {
        let mut editor = EditorState::new(Language::English, "Overlapping transitions");
        editor.add_media_paths([
            PathBuf::from("left.mp4"),
            PathBuf::from("middle.mp4"),
            PathBuf::from("right.mp4"),
        ]);
        let track = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap()
            .id;
        let left = editor
            .timeline
            .insert_clip(track, MediaId(1), Tick(0), Tick(4_000_000), Tick(2_000_000))
            .unwrap();
        let middle = editor
            .timeline
            .insert_clip(
                track,
                MediaId(2),
                Tick(4_000_000),
                Tick(2_000_000),
                Tick(2_000_000),
            )
            .unwrap();
        let right = editor
            .timeline
            .insert_clip(
                track,
                MediaId(3),
                Tick(6_000_000),
                Tick(4_000_000),
                Tick(2_000_000),
            )
            .unwrap();
        editor
            .timeline
            .add_video_transition(track, left, middle, Tick(2_000_000), 0.0)
            .unwrap();
        editor
            .timeline
            .add_video_transition(track, middle, right, Tick(2_000_000), 0.0)
            .unwrap();
        let mut snapshot = editor.snapshot();
        snapshot.timeline.transitions[0].duration = Tick(3_000_000);
        snapshot.timeline.transitions[1].duration = Tick(2_000_000);
        for media in &mut snapshot.media {
            media.duration = Some(Tick(10_000_000));
        }
        let request = ExportRequest {
            snapshot,
            ..request(&editor)
        };
        let error = ExportPlan::from_request_with_probe(&request, probe).unwrap_err();
        assert!(error.contains("overlap inside shared clip"), "{error}");
    }

    #[test]
    fn cross_dissolve_rejects_missing_saved_video_duration() {
        let mut editor = EditorState::new(Language::English, "Transition validation");
        editor.add_media_paths([PathBuf::from("left.mp4"), PathBuf::from("right.mp4")]);
        let track = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap()
            .id;
        let left = editor
            .timeline
            .insert_clip(track, MediaId(1), Tick(0), Tick(2_000_000), Tick(1_000_000))
            .unwrap();
        let right = editor
            .timeline
            .insert_clip(
                track,
                MediaId(2),
                Tick(2_000_000),
                Tick(2_000_000),
                Tick(1_000_000),
            )
            .unwrap();
        editor
            .timeline
            .add_video_transition(track, left, right, Tick(1_000_000), 0.0)
            .unwrap();
        let request = request(&editor);
        let error = ExportPlan::from_request_with_probe(&request, probe).unwrap_err();
        assert!(
            error.contains("transition 1 export is malformed"),
            "{error}"
        );
        assert!(error.contains("saved media duration"), "{error}");
    }

    #[test]
    fn audio_mix_preserves_timing_gain_channel_trim_pan_and_curve() {
        let mut editor = EditorState::new(Language::English, "Export");
        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        assert!(editor.add_selected_to_timeline());
        let mut snapshot = editor.snapshot();
        let track = snapshot
            .timeline
            .tracks
            .iter_mut()
            .find(|track| track.kind == TrackKind::Audio)
            .unwrap();
        track.gain_db = 6.0;
        track.pan = 0.5;
        let clip = &mut track.clips[0];
        clip.start = Tick(2_000_000);
        clip.gain_db = 6.0;
        clip.gain_left_db = -3.0;
        clip.fade_in = Fade {
            duration: Tick(100_000),
            curve: 0.8,
        };
        let request = ExportRequest {
            snapshot,
            ..request(&editor)
        };
        let plan = ExportPlan::from_request_with_probe(&request, probe).unwrap();
        let (args, graph) = build_ffmpeg_job(&request, &plan, H264Encoder::OpenH264).unwrap();
        let left_input = args.iter().rposition(|arg| arg == "clip.mp4").unwrap();
        assert_eq!(
            &args[left_input - 5..left_input],
            ["-ss", "0.000000", "-t", "15.000000", "-i"]
        );
        assert!(graph.contains("pan=stereo|c0="));
        assert!(graph.contains("adelay=96000S:all=1"));
        assert!(graph.contains("volume='if(lt(t\\,0.100000)"));
        assert!(graph.contains("amix=inputs=1:normalize=0"));
    }

    #[test]
    fn equal_power_audio_crossfade_expands_sources_and_lowers_quarter_sine_envelopes() {
        let mut editor = EditorState::new(Language::English, "Audio crossfade export");
        editor.add_media_paths([PathBuf::from("left.mp4"), PathBuf::from("right.mp4")]);
        editor.media[0].duration = Some(Tick(5_000_000));
        editor.media[1].duration = Some(Tick(5_000_000));
        let video_track = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap()
            .id;
        editor
            .timeline
            .insert_clip(video_track, MediaId(1), Tick(0), Tick(4_000_000), Tick(0))
            .unwrap();
        let audio_track = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Audio)
            .unwrap()
            .id;
        let left = editor
            .timeline
            .insert_clip(
                audio_track,
                MediaId(1),
                Tick(0),
                Tick(2_000_000),
                Tick(500_000),
            )
            .unwrap();
        let right = editor
            .timeline
            .insert_clip(
                audio_track,
                MediaId(2),
                Tick(2_000_000),
                Tick(2_000_000),
                Tick(500_000),
            )
            .unwrap();
        editor
            .timeline
            .add_audio_transition(audio_track, left, right, Tick(1_000_000))
            .unwrap();

        let request = request(&editor);
        let plan = ExportPlan::from_request_with_probe(&request, probe).unwrap();
        assert_eq!(plan.audio_clips.len(), 2);
        assert_eq!(plan.audio_clips[0].input_source_in, Tick(500_000));
        assert_eq!(plan.audio_clips[0].input_duration, Tick(2_500_000));
        assert_eq!(plan.audio_clips[0].timeline_start, Tick(0));
        assert_eq!(plan.audio_clips[1].input_source_in, Tick(0));
        assert_eq!(plan.audio_clips[1].input_duration, Tick(2_500_000));
        assert_eq!(plan.audio_clips[1].timeline_start, Tick(1_500_000));
        let (_, graph) = build_ffmpeg_job(&request, &plan, H264Encoder::OpenH264).unwrap();
        assert!(graph.contains("afade=t=out:st=1.500000:d=1.000000:curve=qsin"));
        assert!(graph.contains("afade=t=in:st=0.000000:d=1.000000:curve=qsin"));
        assert!(graph.contains("amix=inputs=2:normalize=0"));
    }

    #[test]
    fn audio_crossfade_rejects_missing_saved_source_handles() {
        let mut editor = EditorState::new(Language::English, "Audio crossfade handles");
        editor.add_media_paths([PathBuf::from("left.mp4"), PathBuf::from("right.mp4")]);
        editor.media[0].duration = Some(Tick(2_000_000));
        editor.media[1].duration = Some(Tick(2_000_000));
        let audio_track = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Audio)
            .unwrap()
            .id;
        let left = editor
            .timeline
            .insert_clip(audio_track, MediaId(1), Tick(0), Tick(2_000_000), Tick(0))
            .unwrap();
        let right = editor
            .timeline
            .insert_clip(
                audio_track,
                MediaId(2),
                Tick(2_000_000),
                Tick(2_000_000),
                Tick(0),
            )
            .unwrap();
        editor
            .timeline
            .add_audio_transition(audio_track, left, right, Tick(1_000_000))
            .unwrap();
        let error = ExportPlan::from_request_with_probe(&request(&editor), probe).unwrap_err();
        assert!(
            error.contains("audio transition 1 export is malformed"),
            "{error}"
        );
        assert!(error.contains("saved frames"), "{error}");
    }

    #[test]
    fn effects_and_excess_layers_are_rejected_not_omitted() {
        let mut editor = EditorState::new(Language::English, "Export");
        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        assert!(editor.add_selected_to_timeline());
        let mut snapshot = editor.snapshot();
        snapshot
            .timeline
            .tracks
            .iter_mut()
            .find(|track| track.kind == TrackKind::Audio)
            .unwrap()
            .effects
            .push(AudioEffect::Normalize);
        let effect_request = ExportRequest {
            snapshot,
            ..request(&editor)
        };
        assert!(
            ExportPlan::from_request_with_probe(&effect_request, probe)
                .unwrap_err()
                .contains("audio effects")
        );
        let mut muted_snapshot = effect_request.snapshot.clone();
        muted_snapshot
            .timeline
            .tracks
            .iter_mut()
            .find(|track| track.kind == TrackKind::Audio)
            .unwrap()
            .muted = true;
        ExportPlan::from_request_with_probe(
            &ExportRequest {
                snapshot: muted_snapshot,
                ..request(&editor)
            },
            probe,
        )
        .expect("effects on an inaudible track must not block an otherwise faithful export");

        let mut editor = EditorState::new(Language::English, "Export");
        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        let mut tracks = editor
            .timeline
            .tracks
            .iter()
            .filter(|track| track.kind == TrackKind::Video)
            .map(|track| track.id)
            .collect::<Vec<_>>();
        while tracks.len() < 5 {
            tracks.push(editor.timeline.add_track(TrackKind::Video));
        }
        for track in tracks {
            editor
                .timeline
                .insert_clip(track, MediaId(1), Tick(0), Tick(1), Tick(0))
                .unwrap();
        }
        assert!(
            ExportPlan::from_request_with_probe(&request(&editor), probe)
                .unwrap_err()
                .contains("at most 4")
        );
    }

    #[test]
    fn audio_rack_filters_keep_clip_then_track_order_and_skip_bypasses() {
        let mut editor = EditorState::new(Language::English, "Audio rack");
        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        assert!(editor.add_selected_to_timeline());
        let mut snapshot = editor.snapshot();
        let track = snapshot
            .timeline
            .tracks
            .iter_mut()
            .find(|track| track.kind == TrackKind::Audio)
            .unwrap();
        track.clips[0].effects = vec![
            AudioEffect::HighPass { hz: 120 },
            AudioEffect::Eq { hz: 1_000, db: 2.5 },
            AudioEffect::Bypassed(Box::new(AudioEffect::Normalize)),
        ];
        track.effects = vec![
            AudioEffect::LowPass { hz: 12_000 },
            AudioEffect::StereoWidth { width: 1.25 },
            AudioEffect::Bypassed(Box::new(AudioEffect::Compressor)),
        ];
        let plan = ExportPlan::from_request_with_probe(
            &ExportRequest {
                snapshot,
                ..request(&editor)
            },
            probe,
        )
        .unwrap();
        let graph = audio_filter(Some(0), &plan.audio_clips[0], "audio");
        let highpass = graph.find("highpass=f=120:t=q:w=0.707").unwrap();
        let equalizer = graph.find("equalizer=f=1000:t=q:w=1:g=2.500000").unwrap();
        let lowpass = graph.find("lowpass=f=12000:t=q:w=0.707").unwrap();
        let width = graph
            .find("pan=stereo|c0=1.125000*c0-0.125000*c1|c1=-0.125000*c0+1.125000*c1")
            .unwrap();
        assert!(highpass < equalizer && equalizer < lowpass && lowpass < width);
        assert!(!graph.contains("Normalize"));
        assert!(!graph.contains("Compressor"));
    }

    #[test]
    fn enabled_unsupported_audio_effects_reject_while_bypassed_ones_export() {
        let mut editor = EditorState::new(Language::English, "Audio rack");
        editor.add_media_paths([PathBuf::from("clip.mp4")]);
        assert!(editor.add_selected_to_timeline());
        let mut snapshot = editor.snapshot();
        snapshot
            .timeline
            .tracks
            .iter_mut()
            .find(|track| track.kind == TrackKind::Audio)
            .unwrap()
            .effects = vec![AudioEffect::Normalize];
        let unsupported = ExportRequest {
            snapshot: snapshot.clone(),
            ..request(&editor)
        };
        assert!(
            ExportPlan::from_request_with_probe(&unsupported, probe)
                .unwrap_err()
                .contains("enabled clip or track audio effects")
        );

        snapshot
            .timeline
            .tracks
            .iter_mut()
            .find(|track| track.kind == TrackKind::Audio)
            .unwrap()
            .effects = vec![AudioEffect::Bypassed(Box::new(AudioEffect::Normalize))];
        ExportPlan::from_request_with_probe(
            &ExportRequest {
                snapshot,
                ..request(&editor)
            },
            probe,
        )
        .expect("a bypassed unsupported effect must not affect export");
    }

    #[test]
    fn bundled_ffmpeg_accepts_the_supported_audio_rack_contract() {
        let _ffmpeg_guard = real_ffmpeg_test_guard();
        let Some(root) = std::env::var_os("FFMPEG_DIR").map(PathBuf::from) else {
            return;
        };
        let ffmpeg = root.join("bin").join(if cfg!(windows) {
            "ffmpeg.exe"
        } else {
            "ffmpeg"
        });
        if !ffmpeg.exists() {
            return;
        }
        let filters = audio_effect_filters(&[
            AudioEffect::HighPass { hz: 120 },
            AudioEffect::Eq { hz: 1_000, db: 2.5 },
            AudioEffect::LowPass { hz: 12_000 },
            AudioEffect::StereoWidth { width: 1.25 },
        ])
        .unwrap();
        let filter_chain = format!("aformat=channel_layouts=stereo{filters}");
        let status = Command::new(ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=48000:duration=0.1",
                "-af",
                &filter_chain,
                "-f",
                "null",
                "-",
            ])
            .status()
            .unwrap();
        assert!(status.success());

        let mono_width = audio_effect_filters(&[AudioEffect::StereoWidth { width: 0.0 }]).unwrap();
        let mono_chain = format!("aformat=channel_layouts=stereo{mono_width}");
        let mono_status = Command::new(root.join("bin").join(if cfg!(windows) {
            "ffmpeg.exe"
        } else {
            "ffmpeg"
        }))
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000:duration=0.1",
            "-af",
            &mono_chain,
            "-f",
            "null",
            "-",
        ])
        .status()
        .unwrap();
        assert!(mono_status.success());
    }

    #[test]
    fn audio_rack_uses_shared_cutoff_ceiling_and_zero_width_emits_mono_samples() {
        let _ffmpeg_guard = real_ffmpeg_test_guard();
        let ceiling = audio_effect_filters(&[AudioEffect::HighPass { hz: 96_000 }]).unwrap();
        assert_eq!(ceiling, ",highpass=f=20000:t=q:w=0.707");

        let Some(root) = std::env::var_os("FFMPEG_DIR").map(PathBuf::from) else {
            return;
        };
        let ffmpeg = root.join("bin").join(if cfg!(windows) {
            "ffmpeg.exe"
        } else {
            "ffmpeg"
        });
        if !ffmpeg.exists() {
            return;
        }
        let width = audio_effect_filters(&[AudioEffect::StereoWidth { width: 0.0 }]).unwrap();
        let chain = format!("aformat=channel_layouts=stereo{width}");
        let output = Command::new(ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "aevalsrc=sin(2*PI*440*t)|cos(2*PI*330*t):s=48000:d=0.02",
                "-af",
                &chain,
                "-f",
                "f32le",
                "-acodec",
                "pcm_f32le",
                "-",
            ])
            .output()
            .unwrap();
        assert!(output.status.success());
        let mut frames = 0;
        for frame in output.stdout.chunks_exact(8) {
            let left = f32::from_le_bytes(frame[0..4].try_into().unwrap());
            let right = f32::from_le_bytes(frame[4..8].try_into().unwrap());
            assert!((left - right).abs() < 0.000_001);
            frames += 1;
        }
        assert!(frames > 100);
    }

    #[test]
    fn compositor_fade_contract_remains_visible_to_export() {
        assert_eq!(video_fade_opacity(0.0), 0.0);
        assert_eq!(video_fade_opacity(1.0), 1.0);
    }

    #[test]
    fn title_only_export_uses_sorted_plates_and_extends_sequence_duration() {
        let mut editor = EditorState::new(Language::English, "Title export");
        let lower = editor
            .timeline
            .add_title(Tick(2_000_000), Tick(1_000_000), "lower")
            .unwrap();
        let upper = editor
            .timeline
            .add_title(Tick(1_000_000), Tick(1_000_000), "日本語")
            .unwrap();
        let mut lower_title = editor.timeline.title(lower).unwrap().clone();
        lower_title.z_order = -1;
        lower_title.position_x = 0.25;
        lower_title.position_y = 0.75;
        editor.timeline.replace_title(lower, lower_title).unwrap();
        let mut upper_title = editor.timeline.title(upper).unwrap().clone();
        upper_title.z_order = 2;
        upper_title.opacity = 0.8;
        upper_title.fade_in = Tick(250_000);
        upper_title.fade_out = Tick(500_000);
        editor.timeline.replace_title(upper, upper_title).unwrap();

        let request = request(&editor);
        let plan = ExportPlan::from_request_with_probe(&request, probe).unwrap();
        assert_eq!(plan.duration, Tick(3_000_000));
        assert_eq!(
            plan.titles
                .iter()
                .map(|title| title.title.id)
                .collect::<Vec<_>>(),
            [lower, upper]
        );
        let assets = plan
            .titles
            .iter()
            .enumerate()
            .map(|(index, planned)| TitleAsset {
                title: planned.title.clone(),
                path: PathBuf::from(format!("title-{index}.tga")),
            })
            .collect::<Vec<_>>();
        let (args, graph) =
            build_ffmpeg_job_with_title_assets(&request, &plan, H264Encoder::OpenH264, &assets)
                .unwrap();
        assert!(args.windows(2).any(|pair| pair == ["-i", "title-0.tga"]));
        assert!(args.windows(2).any(|pair| pair == ["-i", "title-1.tga"]));
        assert!(
            graph.contains(
                "[vbase0][title0]overlay=x='480.000-overlay_w/2':y='810.000-overlay_h/2'"
            ),
            "{graph}"
        );
        assert!(graph.contains("[vtitle1][title1]overlay"), "{graph}");
        assert!(graph.contains("trim=duration=1.000000"), "{graph}");
        assert!(graph.contains("setpts=PTS+2.000000/TB[title0]"), "{graph}");
        assert!(
            graph.contains(
                "[vbase0][title0]overlay=x='480.000-overlay_w/2':y='810.000-overlay_h/2':format=rgb:alpha=straight:"
            ),
            "{graph}"
        );
        assert!(
            graph.contains("min(0.800000\\,if(lt(T\\,0.250000)\\,T/0.250000\\,1))"),
            "{graph}"
        );
        assert!(
            graph.contains("if(gt(T\\,0.500000)\\,(1.000000-T)/0.500000\\,1)"),
            "{graph}"
        );
    }

    #[test]
    fn title_envelope_matches_shared_linear_contract_at_key_boundaries() {
        let title = TitleOverlay {
            id: TitleId(1),
            start: Tick(0),
            duration: Tick(1_000_000),
            text: "Title".into(),
            fade_in: Tick(250_000),
            fade_out: Tick(500_000),
            opacity: 0.8,
            ..TitleOverlay::default()
        };
        assert_eq!(title_fade_opacity(&title, Tick(0)), 0.0);
        assert!((title_fade_opacity(&title, Tick(125_000)) - 0.4).abs() < 0.0001);
        assert!((title_fade_opacity(&title, Tick(500_000)) - 0.8).abs() < 0.0001);
        assert!((title_fade_opacity(&title, Tick(750_000)) - 0.4).abs() < 0.0001);
        assert_eq!(title_fade_opacity(&title, Tick(1_000_000)), 0.0);
        assert_eq!(
            title_opacity_expression(&title),
            "min(min(0.800000\\,if(lt(T\\,0.250000)\\,T/0.250000\\,1))\\,if(gt(T\\,0.500000)\\,(1.000000-T)/0.500000\\,1))"
        );
    }

    #[test]
    fn disabled_title_does_not_create_a_blank_export_tail() {
        let mut editor = EditorState::new(Language::English, "Disabled title");
        let title = editor
            .timeline
            .add_title(Tick(9_000_000), Tick(1_000_000), "hidden")
            .unwrap();
        let mut title = editor.timeline.title(title).unwrap().clone();
        title.enabled = false;
        editor.timeline.replace_title(title.id, title).unwrap();
        assert!(
            ExportPlan::from_request_with_probe(&request(&editor), probe)
                .unwrap_err()
                .contains("no unmuted video clips or enabled titles")
        );
    }

    #[test]
    fn disabled_clips_are_omitted_and_bypass_their_transitions() {
        let mut editor = EditorState::new(Language::English, "Disabled clips");
        editor.add_media_paths([PathBuf::from("kept.mp4"), PathBuf::from("disabled.mp4")]);
        let video_track = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap()
            .id;
        let kept_video = editor
            .timeline
            .insert_clip(
                video_track,
                MediaId(1),
                Tick(0),
                Tick(2_000_000),
                Tick(1_000_000),
            )
            .unwrap();
        let disabled_video = editor
            .timeline
            .insert_clip(
                video_track,
                MediaId(2),
                Tick(2_000_000),
                Tick(2_000_000),
                Tick(1_000_000),
            )
            .unwrap();
        editor
            .timeline
            .add_video_transition(
                video_track,
                kept_video,
                disabled_video,
                Tick(1_000_000),
                0.0,
            )
            .unwrap();
        editor
            .timeline
            .set_clip_enabled(disabled_video, false, false)
            .unwrap();

        let audio_track = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Audio)
            .unwrap()
            .id;
        let kept_audio = editor
            .timeline
            .insert_clip(
                audio_track,
                MediaId(1),
                Tick(0),
                Tick(2_000_000),
                Tick(1_000_000),
            )
            .unwrap();
        let disabled_audio = editor
            .timeline
            .insert_clip(
                audio_track,
                MediaId(2),
                Tick(2_000_000),
                Tick(2_000_000),
                Tick(1_000_000),
            )
            .unwrap();
        editor
            .timeline
            .add_audio_transition(audio_track, kept_audio, disabled_audio, Tick(1_000_000))
            .unwrap();
        editor
            .timeline
            .set_clip_enabled(disabled_audio, false, false)
            .unwrap();

        let request = request(&editor);
        let plan = ExportPlan::from_request_with_probe(&request, probe).unwrap();
        assert_eq!(plan.video_tracks.len(), 1);
        assert_eq!(plan.video_tracks[0].clips.len(), 1);
        assert_eq!(plan.video_tracks[0].clips[0].clip.id, kept_video);
        assert_eq!(plan.video_tracks[0].clips[0].timeline_end, Tick(2_000_000));
        assert_eq!(plan.audio_clips.len(), 1);
        assert_eq!(plan.audio_clips[0].clip.id, kept_audio);
        assert_eq!(plan.audio_clips[0].input_duration, Tick(2_000_000));
        assert_eq!(plan.duration, Tick(4_000_000));

        let (args, graph) = build_ffmpeg_job(&request, &plan, H264Encoder::OpenH264).unwrap();
        assert!(!args.iter().any(|arg| arg == "disabled.mp4"), "{args:?}");
        assert!(graph.contains("trim=duration=2.000000"), "{graph}");
    }

    #[test]
    fn real_bundled_ffmpeg_decodes_alpha_title_assets_at_the_planned_interval() {
        let _ffmpeg_guard = real_ffmpeg_test_guard();
        let Some(root) = std::env::var_os("FFMPEG_DIR").map(PathBuf::from) else {
            return;
        };
        let ffmpeg = root.join("bin").join(if cfg!(windows) {
            "ffmpeg.exe"
        } else {
            "ffmpeg"
        });
        if !ffmpeg.exists() {
            return;
        }
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let output = std::env::temp_dir().join(format!("maelstrom-title-output-{nonce}.mp4"));
        let filter = std::env::temp_dir().join(format!("maelstrom-title-{nonce}.filter"));

        let mut editor = EditorState::new(Language::English, "Real title export");
        let id = editor
            .timeline
            .add_title(Tick(250_000), Tick(750_000), "TITLE")
            .unwrap();
        let mut title = editor.timeline.title(id).unwrap().clone();
        title.font_size = 72.0;
        title.fill = TitleColor::rgba(255, 0, 0, 255);
        title.outline_width = 0.0;
        title.shadow_color = TitleColor::rgba(0, 0, 0, 0);
        title.position_x = 0.5;
        title.position_y = 0.5;
        editor.timeline.replace_title(id, title).unwrap();
        let request = ExportRequest {
            snapshot: editor.snapshot(),
            settings: ProjectSettings {
                fps: [20, 1],
                size: [320, 180],
            },
            output: output.clone(),
            ffmpeg: ffmpeg.clone(),
            encoders: vec![H264Encoder::OpenH264],
        };
        let plan = ExportPlan::from_request_with_probe(&request, probe).unwrap();
        let cancel = AtomicBool::new(false);
        let assets = materialize_title_assets(&request, &plan, &cancel).unwrap();
        assert_eq!(assets.len(), 1);
        assert_eq!(
            assets[0].path.extension().and_then(|ext| ext.to_str()),
            Some("tga")
        );
        assert!(assets[0].path.exists());

        let (mut args, graph) =
            build_ffmpeg_job_with_title_assets(&request, &plan, H264Encoder::OpenH264, &assets)
                .unwrap();
        let encoder = args.iter().position(|arg| arg == "-c:v").unwrap();
        args[encoder + 1] = "mpeg4".to_owned();
        fs::write(&filter, graph).unwrap();
        let (events, _) = mpsc::channel();
        let notify: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});
        let result = run_child(
            &ffmpeg,
            &args,
            &filter,
            plan.duration,
            &cancel,
            &events,
            &notify,
        );
        cleanup_title_assets(&assets);
        assert!(assets.iter().all(|asset| !asset.path.exists()));
        let _ = fs::remove_file(&filter);
        assert!(result.is_ok(), "{result:?}");

        let sample = |seconds: &str| {
            Command::new(&ffmpeg)
                .args(["-v", "error", "-ss", seconds, "-i"])
                .arg(&output)
                .args([
                    "-frames:v",
                    "1",
                    "-f",
                    "rawvideo",
                    "-pix_fmt",
                    "rgb24",
                    "pipe:1",
                ])
                .output()
                .unwrap()
        };
        let outside = sample("0.100");
        assert!(outside.status.success());
        assert!(
            outside
                .stdout
                .chunks_exact(3)
                .all(|pixel| pixel[0] < 25 && pixel[1] < 25 && pixel[2] < 25)
        );
        let inside = sample("0.500");
        assert!(inside.status.success());
        assert!(
            inside
                .stdout
                .chunks_exact(3)
                .any(|pixel| pixel[0] > 120 && pixel[0] > pixel[1] * 2 && pixel[0] > pixel[2] * 2)
        );
        let _ = fs::remove_file(&output);
    }

    #[test]
    fn preflight_failure_never_deletes_an_existing_destination() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let output = std::env::temp_dir().join(format!("maelstrom-existing-{nonce}.mp4"));
        fs::write(&output, b"existing user file").unwrap();
        let mut editor = EditorState::new(Language::English, "Safe failure");
        editor.add_media_paths([PathBuf::from("missing-source.mp4")]);
        assert!(editor.add_selected_to_timeline());
        let job = ExportJob::start(
            ExportRequest {
                snapshot: editor.snapshot(),
                settings: ProjectSettings::default(),
                output: output.clone(),
                ffmpeg: PathBuf::from("missing-tools/ffmpeg"),
                encoders: vec![H264Encoder::OpenH264],
            },
            || {},
        )
        .unwrap();
        let terminal = loop {
            if let Ok(event) = job.try_recv()
                && !matches!(event, ExportEvent::Progress(_))
            {
                break event;
            }
            thread::sleep(Duration::from_millis(5));
        };
        assert!(matches!(terminal, ExportEvent::Failed(_)));
        assert_eq!(fs::read(&output).unwrap(), b"existing user file");
        fs::remove_file(output).unwrap();
    }

    #[test]
    fn real_ffmpeg_parses_and_runs_the_transformed_layer_graph() {
        let _ffmpeg_guard = real_ffmpeg_test_guard();
        let Some(root) = std::env::var_os("FFMPEG_DIR").map(PathBuf::from) else {
            return;
        };
        let ffmpeg = root.join("bin").join(if cfg!(windows) {
            "ffmpeg.exe"
        } else {
            "ffmpeg"
        });
        if !ffmpeg.exists() {
            return;
        }
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let media = std::env::temp_dir().join(format!("maelstrom-export-input-{nonce}.mp4"));
        let output = std::env::temp_dir().join(format!("maelstrom-export-output-{nonce}.mp4"));
        let generated = Command::new(&ffmpeg)
            .args([
                "-hide_banner",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=320x180:rate=24",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:sample_rate=48000",
                "-t",
                "1",
                "-c:v",
                "mpeg4",
                "-c:a",
                "aac",
            ])
            .arg(&media)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(generated.success());
        let mut editor = EditorState::new(Language::English, "FFmpeg graph");
        editor.add_media_paths([media.clone()]);
        assert!(editor.add_selected_to_timeline());
        let mut snapshot = editor.snapshot();
        for track in &mut snapshot.timeline.tracks {
            for clip in &mut track.clips {
                clip.duration = Tick(1_000_000);
                clip.fade_in = Fade {
                    duration: Tick(250_000),
                    curve: 0.5,
                };
                clip.fade_out = Fade {
                    duration: Tick(250_000),
                    curve: -0.5,
                };
            }
        }
        snapshot
            .timeline
            .tracks
            .iter_mut()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap()
            .clips[0]
            .transform = ClipTransform {
            rotation_degrees: 15.0,
            scale_x: 0.8,
            scale_y: 0.9,
            crop_left: 0.05,
            crop_right: 0.05,
            opacity: 0.8,
            ..Default::default()
        };
        snapshot
            .timeline
            .tracks
            .iter_mut()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap()
            .clips[0]
            .video_effects
            .push(brightness_contrast_node(
                true,
                AnimatedScalar {
                    value: 0.1,
                    keyframes: vec![
                        ScalarKeyframe {
                            source_tick: Tick(0),
                            value: 0.1,
                            interpolation: KeyframeInterpolation::Smooth,
                        },
                        ScalarKeyframe {
                            source_tick: Tick(250_000),
                            value: 0.14,
                            interpolation: KeyframeInterpolation::EaseIn,
                        },
                        ScalarKeyframe {
                            source_tick: Tick(500_000),
                            value: 0.17,
                            interpolation: KeyframeInterpolation::EaseOut,
                        },
                        ScalarKeyframe {
                            source_tick: Tick(750_000),
                            value: 0.2,
                            interpolation: KeyframeInterpolation::Hold,
                        },
                        ScalarKeyframe {
                            source_tick: Tick(1_000_000),
                            value: 0.2,
                            interpolation: KeyframeInterpolation::Linear,
                        },
                    ],
                },
                scalar(1.2),
            ))
            .unwrap();
        let request = ExportRequest {
            snapshot,
            settings: ProjectSettings {
                fps: [24, 1],
                size: [320, 180],
            },
            output: output.clone(),
            ffmpeg: ffmpeg.clone(),
            encoders: vec![H264Encoder::OpenH264],
        };
        let plan = ExportPlan::from_request_with_probe(&request, |_| {
            Ok(MediaProbe {
                source_size: Some(PixelSize::new(320, 180)),
                has_audio: true,
                ..Default::default()
            })
        })
        .unwrap();
        let (mut args, graph) = build_ffmpeg_job(&request, &plan, H264Encoder::OpenH264).unwrap();
        let encoder = args.iter().position(|arg| arg == "-c:v").unwrap();
        args[encoder + 1] = "mpeg4".to_owned();
        let filter = std::env::temp_dir().join(format!("maelstrom-export-{nonce}.filter"));
        fs::write(&filter, &graph).unwrap();
        let cancel = AtomicBool::new(false);
        let (events, _) = mpsc::channel();
        let notify: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});
        let result = run_child(
            &ffmpeg,
            &args,
            &filter,
            plan.duration,
            &cancel,
            &events,
            &notify,
        );
        let _ = fs::remove_file(&filter);
        let _ = fs::remove_file(&media);
        assert!(result.is_ok(), "{result:?}");
        assert!(output.exists());
        let _ = fs::remove_file(&output);
    }

    #[test]
    fn real_ffmpeg_freezes_a_still_image_for_its_planned_duration() {
        let _ffmpeg_guard = real_ffmpeg_test_guard();
        let Some(root) = std::env::var_os("FFMPEG_DIR").map(PathBuf::from) else {
            return;
        };
        let ffmpeg = root.join("bin").join(if cfg!(windows) {
            "ffmpeg.exe"
        } else {
            "ffmpeg"
        });
        let ffprobe = root.join("bin").join(if cfg!(windows) {
            "ffprobe.exe"
        } else {
            "ffprobe"
        });
        if !ffmpeg.exists() || !ffprobe.exists() {
            return;
        }
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let still = std::env::temp_dir().join(format!("maelstrom-still-input-{nonce}.bmp"));
        let output = std::env::temp_dir().join(format!("maelstrom-still-output-{nonce}.mp4"));
        let generated = Command::new(&ffmpeg)
            .args([
                "-hide_banner",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=red:s=32x18:r=1",
                "-frames:v",
                "1",
                "-c:v",
                "bmp",
            ])
            .arg(&still)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(generated.success());

        let mut editor = EditorState::new(Language::English, "Still FFmpeg export");
        editor.add_media_paths([still.clone()]);
        let track = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap()
            .id;
        editor
            .timeline
            .insert_clip(track, MediaId(1), Tick(0), Tick(1_000_000), Tick(0))
            .unwrap();
        let request = ExportRequest {
            snapshot: editor.snapshot(),
            settings: ProjectSettings {
                fps: [24, 1],
                size: [32, 18],
            },
            output: output.clone(),
            ffmpeg: ffmpeg.clone(),
            encoders: vec![H264Encoder::OpenH264],
        };
        let plan = ExportPlan::from_request(&request).unwrap();
        assert!(plan.video_tracks[0].clips[0].is_still);
        let (mut args, graph) = build_ffmpeg_job(&request, &plan, H264Encoder::OpenH264).unwrap();
        let encoder = args.iter().position(|arg| arg == "-c:v").unwrap();
        args[encoder + 1] = "mpeg4".to_owned();
        let filter = std::env::temp_dir().join(format!("maelstrom-still-{nonce}.filter"));
        fs::write(&filter, &graph).unwrap();
        let cancel = AtomicBool::new(false);
        let (events, _) = mpsc::channel();
        let notify: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});
        let result = run_child(
            &ffmpeg,
            &args,
            &filter,
            plan.duration,
            &cancel,
            &events,
            &notify,
        );
        let _ = fs::remove_file(&filter);
        assert!(result.is_ok(), "{result:?}");

        let duration = Command::new(&ffprobe)
            .args([
                "-v",
                "error",
                "-show_entries",
                "format=duration",
                "-of",
                "default=noprint_wrappers=1:nokey=1",
            ])
            .arg(&output)
            .output()
            .unwrap();
        assert!(duration.status.success());
        let duration = String::from_utf8_lossy(&duration.stdout)
            .trim()
            .parse::<f64>()
            .unwrap();
        assert!(
            (duration - 1.0).abs() < 0.05,
            "unexpected duration: {duration}"
        );

        let pixel = Command::new(&ffmpeg)
            .args(["-v", "error", "-ss", "0.500", "-i"])
            .arg(&output)
            .args([
                "-frames:v",
                "1",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgb24",
                "pipe:1",
            ])
            .output()
            .unwrap();
        assert!(pixel.status.success());
        assert!(pixel.stdout.len() >= 3);
        assert!(pixel.stdout[0] > 200 && pixel.stdout[1] < 30 && pixel.stdout[2] < 30);

        let _ = fs::remove_file(&still);
        let _ = fs::remove_file(&output);
    }

    #[test]
    fn real_ffmpeg_premultiplied_filtering_preserves_transparent_edge_energy() {
        let _ffmpeg_guard = real_ffmpeg_test_guard();
        let Some(root) = std::env::var_os("FFMPEG_DIR").map(PathBuf::from) else {
            return;
        };
        let ffmpeg = root.join("bin").join(if cfg!(windows) {
            "ffmpeg.exe"
        } else {
            "ffmpeg"
        });
        if !ffmpeg.exists() {
            return;
        }
        let graph = concat!(
            "color=c=black:s=64x64:r=1[base];",
            "color=c=black@0:s=16x16:r=1,format=rgba,",
            "geq=r='if(lt(X,8),255,0)':g=0:b=0:a='if(lt(X,8),255,0)',",
            "premultiply=inplace=1,scale=64:64,unpremultiply=inplace=1[layer];",
            "[base][layer]overlay=alpha=straight,format=rgb24[out]"
        );
        let output = Command::new(&ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "nullsrc=s=64x64:r=1",
                "-filter_complex",
                graph,
                "-map",
                "[out]",
                "-frames:v",
                "1",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgb24",
                "pipe:1",
            ])
            .output()
            .expect("run premultiplied FFmpeg edge contract");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout.len(), 64 * 64 * 3);
        let red = |x: usize| output.stdout[(32 * 64 + x) * 3];
        assert!(
            red(31) >= 160 && red(32) >= 30 && red(35) <= 2,
            "premultiplied edge lost filtered color energy: x31={}, x32={}, x35={}",
            red(31),
            red(32),
            red(35)
        );
    }

    #[test]
    fn generated_still_graph_preserves_transparent_edge_energy() {
        let _ffmpeg_guard = real_ffmpeg_test_guard();
        let Some(root) = std::env::var_os("FFMPEG_DIR").map(PathBuf::from) else {
            return;
        };
        let ffmpeg = root.join("bin").join(if cfg!(windows) {
            "ffmpeg.exe"
        } else {
            "ffmpeg"
        });
        if !ffmpeg.exists() {
            return;
        }
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp = std::env::temp_dir();
        // The approved developer bundle intentionally omits the PNG decoder; BMP preserves this
        // RGBA fixture and is supported by both the media classifier and pinned FFmpeg runtime.
        let still = temp.join(format!("maelstrom-alpha-edge-{nonce}.bmp"));
        let filter = temp.join(format!("maelstrom-alpha-edge-{nonce}.filter"));
        let output = temp.join(format!("maelstrom-alpha-edge-{nonce}.mp4"));
        let image = image::RgbaImage::from_fn(16, 16, |x, _| {
            if x < 8 {
                image::Rgba([255, 0, 0, 255])
            } else {
                image::Rgba([0, 0, 0, 0])
            }
        });
        image.save(&still).expect("write alpha-edge still");

        let mut editor = EditorState::new(Language::English, "Alpha edge export");
        editor.add_media_paths([still.clone()]);
        let track = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap()
            .id;
        editor
            .timeline
            .insert_clip(track, MediaId(1), Tick(0), Tick(1_000_000), Tick(0))
            .unwrap();
        let request = ExportRequest {
            snapshot: editor.snapshot(),
            settings: ProjectSettings {
                fps: [24, 1],
                size: [64, 64],
            },
            output: output.clone(),
            ffmpeg: ffmpeg.clone(),
            encoders: vec![H264Encoder::OpenH264],
        };
        let plan = ExportPlan::from_request(&request).expect("plan alpha-edge still");
        let (mut args, graph) = build_ffmpeg_job(&request, &plan, H264Encoder::OpenH264).unwrap();
        let encoder = args.iter().position(|arg| arg == "-c:v").unwrap();
        args[encoder + 1] = "mpeg4".to_owned();
        fs::write(&filter, &graph).unwrap();
        let cancel = AtomicBool::new(false);
        let (events, _) = mpsc::channel();
        let notify: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});
        let render = run_child(
            &ffmpeg,
            &args,
            &filter,
            plan.duration,
            &cancel,
            &events,
            &notify,
        );
        let sample = Command::new(&ffmpeg)
            .args(["-hide_banner", "-loglevel", "error", "-ss", "0.5", "-i"])
            .arg(&output)
            .args([
                "-vf",
                "format=rgb24,crop=2:1:31:32",
                "-frames:v",
                "1",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgb24",
                "pipe:1",
            ])
            .output()
            .expect("sample generated alpha-edge export");
        for path in [&filter, &still, &output] {
            let _ = fs::remove_file(path);
        }
        assert!(render.is_ok(), "{render:?}\n{graph}");
        assert!(
            sample.status.success(),
            "{}",
            String::from_utf8_lossy(&sample.stderr)
        );
        assert_eq!(sample.stdout.len(), 6);
        assert!(
            sample.stdout[0] >= 150 && sample.stdout[3] >= 30,
            "generated alpha edge lost filtered energy: {:?}\n{graph}",
            sample.stdout
        );
    }

    #[test]
    fn real_ffmpeg_color_pixels_match_the_encoded_rgb_contract() {
        let _ffmpeg_guard = real_ffmpeg_test_guard();
        let Some(root) = std::env::var_os("FFMPEG_DIR").map(PathBuf::from) else {
            return;
        };
        let ffmpeg = root.join("bin").join(if cfg!(windows) {
            "ffmpeg.exe"
        } else {
            "ffmpeg"
        });
        if !ffmpeg.exists() {
            return;
        }

        let mut editor = EditorState::new(Language::English, "Color pixel contract");
        editor.add_media_paths([PathBuf::from("color-contract.mp4")]);
        assert!(editor.add_selected_to_timeline());
        let mut clip = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap()
            .clips[0]
            .clone();
        let brightness = 0.12;
        let contrast = 1.35;
        let temperature = 0.2;
        let tint = -0.15;
        let saturation = 1.2;
        let exposure = 0.35;
        let highlights = 0.25;
        let shadows = -0.2;
        let whites = 0.4;
        let blacks = -0.3;
        clip.video_effects = vec![brightness_contrast_node(
            true,
            scalar(brightness),
            scalar(contrast),
        )]
        .into();
        let mut curves = None;
        assert!(
            clip.video_effects
                .edit(0, |node| {
                    let VideoEffectKind::BrightnessContrast(effect) = &mut node.kind else {
                        panic!("expected the basic correction operation");
                    };
                    effect.temperature = scalar(temperature);
                    effect.tint = scalar(tint);
                    effect.saturation = scalar(saturation);
                    effect.exposure = scalar(exposure);
                    effect.highlights = scalar(highlights);
                    effect.shadows = scalar(shadows);
                    effect.whites = scalar(whites);
                    effect.blacks = scalar(blacks);
                    effect.curves.red = ColorCurve {
                        points: vec![
                            CurvePoint { x: 0.0, y: 0.0 },
                            CurvePoint { x: 0.5, y: 0.72 },
                            CurvePoint { x: 1.0, y: 1.0 },
                        ],
                    };
                    effect.curves.master = ColorCurve {
                        points: vec![
                            CurvePoint { x: 0.0, y: 0.0 },
                            CurvePoint { x: 0.5, y: 0.43 },
                            CurvePoint { x: 1.0, y: 1.0 },
                        ],
                    };
                    curves = Some(effect.curves.clone());
                })
                .is_ok()
        );
        let curves = curves.unwrap();
        let correction = video_color_filter(&clip).unwrap();
        let sample = |filter: &str| -> Vec<u8> {
            let output = Command::new(&ffmpeg)
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "color=c=0x4080c0:s=4x4:r=1:d=1,format=rgba",
                    "-vf",
                    filter,
                    "-frames:v",
                    "1",
                    "-f",
                    "rawvideo",
                    "-pix_fmt",
                    "rgba",
                    "pipe:1",
                ])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "FFmpeg color sample failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            output.stdout
        };
        let baseline = sample("format=rgba");
        let corrected = sample(&format!("format=rgba{correction}"));
        assert_eq!(baseline.len(), 4 * 4 * 4);
        assert_eq!(corrected.len(), baseline.len());
        let mut encoded = [
            baseline[0] as f32 / 255.0 + 0.10 * temperature + 0.05 * tint,
            baseline[1] as f32 / 255.0 - 0.05 * tint,
            baseline[2] as f32 / 255.0 - 0.10 * temperature + 0.05 * tint,
        ];
        let exposure_scale = 2.0_f32.powf(exposure);
        for component in &mut encoded {
            *component *= exposure_scale;
        }
        let luma = 0.2126 * encoded[0] + 0.7152 * encoded[1] + 0.0722 * encoded[2];
        for component in &mut encoded {
            *component =
                (luma + (*component - luma) * saturation - 0.5) * contrast + 0.5 + brightness;
        }
        let luma =
            (0.2126 * encoded[0] + 0.7152 * encoded[1] + 0.0722 * encoded[2]).clamp(0.0, 1.0);
        for (channel, component) in encoded.into_iter().enumerate() {
            let basic = (component
                + 0.25 * highlights * luma * luma
                + 0.25 * shadows * (1.0 - luma) * (1.0 - luma)
                + 0.20 * whites * luma.powi(8)
                + 0.20 * blacks * (1.0 - luma).powi(8))
            .clamp(0.0, 1.0);
            let component_curve = match channel {
                0 => &curves.red,
                1 => &curves.green,
                _ => &curves.blue,
            };
            let expected =
                (curves.master.sample(component_curve.sample(basic)) * 255.0).round() as i16;
            assert!(
                (i16::from(corrected[channel]) - expected).abs() <= 3,
                "channel {channel}: baseline={}, corrected={}, expected={expected}",
                baseline[channel],
                corrected[channel]
            );
        }
        assert_eq!(corrected[3], baseline[3], "color correction changed alpha");
    }

    #[test]
    fn real_ffmpeg_color_stack_preserves_intermediate_clamps() {
        let _ffmpeg_guard = real_ffmpeg_test_guard();
        let Some(root) = std::env::var_os("FFMPEG_DIR").map(PathBuf::from) else {
            return;
        };
        let ffmpeg = root.join("bin").join(if cfg!(windows) {
            "ffmpeg.exe"
        } else {
            "ffmpeg"
        });
        if !ffmpeg.exists() {
            return;
        }

        let mut editor = EditorState::new(Language::English, "Color stack clamp");
        editor.add_media_paths([PathBuf::from("color-stack.mp4")]);
        assert!(editor.add_selected_to_timeline());
        let mut clip = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap()
            .clips[0]
            .clone();
        let mut second = brightness_contrast_node(true, scalar(-0.5), scalar(1.0));
        second.id = VideoEffectId(2);
        clip.video_effects = vec![
            brightness_contrast_node(true, scalar(0.8), scalar(1.0)),
            second,
        ]
        .into();
        let sample = |filter: &str| -> Vec<u8> {
            let output = Command::new(&ffmpeg)
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "color=c=0x204060:s=4x4:r=1:d=1,format=rgba",
                    "-vf",
                    filter,
                    "-frames:v",
                    "1",
                    "-f",
                    "rawvideo",
                    "-pix_fmt",
                    "rgba",
                    "pipe:1",
                ])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "FFmpeg stacked color sample failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            output.stdout
        };
        let baseline = sample("format=rgba");
        let corrected = sample(&format!(
            "format=rgba{}",
            video_color_filter(&clip).unwrap()
        ));
        assert_eq!(corrected.len(), baseline.len());
        for channel in 0..3 {
            let encoded = baseline[channel] as f32 / 255.0;
            let after_first = (encoded + 0.8).clamp(0.0, 1.0);
            let expected = ((after_first - 0.5).clamp(0.0, 1.0) * 255.0).round() as i16;
            assert!(
                (i16::from(corrected[channel]) - expected).abs() <= 2,
                "channel {channel}: corrected={}, expected={expected}",
                corrected[channel]
            );
        }
        assert_eq!(corrected[3], baseline[3], "color stack changed alpha");
    }

    #[test]
    fn real_ffmpeg_vignette_darkens_edges_in_stack_order_and_skips_bypass() {
        let _ffmpeg_guard = real_ffmpeg_test_guard();
        let Some(root) = std::env::var_os("FFMPEG_DIR").map(PathBuf::from) else {
            return;
        };
        let ffmpeg = root.join("bin").join(if cfg!(windows) {
            "ffmpeg.exe"
        } else {
            "ffmpeg"
        });
        if !ffmpeg.exists() {
            return;
        }

        let mut editor = EditorState::new(Language::English, "Vignette export");
        editor.add_media_paths([PathBuf::from("vignette.mp4")]);
        assert!(editor.add_selected_to_timeline());
        let mut clip = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap()
            .clips[0]
            .clone();
        clip.video_effects = vec![
            brightness_contrast_node(true, scalar(0.1), scalar(1.0)),
            vignette_node(2, true, 0.8),
            vignette_node(3, false, 1.0),
        ]
        .into();
        let filter = video_color_filter(&clip).unwrap();
        let color = filter.find(",geq=").unwrap();
        let vignette = filter.rfind(",geq=").unwrap();
        assert!(color < vignette, "vignette must follow prior enabled nodes");
        assert_eq!(
            filter.matches(",geq=").count(),
            2,
            "bypassed node leaked into graph"
        );
        assert_eq!(filter.matches("sqrt(ld(0)*ld(0)+ld(1)*ld(1))").count(), 3);

        let output = Command::new(ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=0x808080:s=64x64:r=1:d=1,format=rgba",
                "-vf",
                &format!("format=rgba{filter}"),
                "-frames:v",
                "1",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgba",
                "pipe:1",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "FFmpeg vignette sample failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let pixel = |x: usize, y: usize| &output.stdout[(y * 64 + x) * 4..(y * 64 + x) * 4 + 4];
        let center = pixel(32, 32);
        let edge = pixel(0, 0);
        assert!(edge[0] < center[0], "edge={edge:?}, center={center:?}");
        assert_eq!(edge[3], center[3], "vignette changed alpha");
    }

    #[test]
    fn vignette_filter_uses_animated_absolute_source_time() {
        let effect = VignetteEffect {
            amount: AnimatedScalar {
                value: 0.35,
                keyframes: vec![
                    ScalarKeyframe {
                        source_tick: Tick(1_000_000),
                        value: 0.0,
                        interpolation: KeyframeInterpolation::Linear,
                    },
                    ScalarKeyframe {
                        source_tick: Tick(2_000_000),
                        value: 1.0,
                        interpolation: KeyframeInterpolation::Linear,
                    },
                ],
            },
            ..Default::default()
        };
        let filter = vignette_filter(&effect, Tick(1_000_000));
        assert!(filter.contains("(T+1.000000)"));
        assert!(filter.contains("sqrt(ld(0)*ld(0)+ld(1)*ld(1))/sqrt(2)"));
    }

    #[test]
    fn real_ffmpeg_easing_pixels_match_timeline_evaluation() {
        let _ffmpeg_guard = real_ffmpeg_test_guard();
        let Some(root) = std::env::var_os("FFMPEG_DIR").map(PathBuf::from) else {
            return;
        };
        let ffmpeg = root.join("bin").join(if cfg!(windows) {
            "ffmpeg.exe"
        } else {
            "ffmpeg"
        });
        if !ffmpeg.exists() {
            return;
        }

        let mut editor = EditorState::new(Language::English, "Easing pixel contract");
        editor.add_media_paths([PathBuf::from("easing-contract.mp4")]);
        assert!(editor.add_selected_to_timeline());
        let mut clip = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap()
            .clips[0]
            .clone();
        let sample = |filter: &str| -> Vec<u8> {
            let output = Command::new(&ffmpeg)
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "color=c=0x204060:s=2x2:r=8:d=1,format=rgba",
                    "-vf",
                    filter,
                    "-frames:v",
                    "8",
                    "-f",
                    "rawvideo",
                    "-pix_fmt",
                    "rgba",
                    "pipe:1",
                ])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "FFmpeg easing sample failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            output.stdout
        };
        let baseline = sample("format=rgba");
        let frame_bytes = 2 * 2 * 4;
        assert_eq!(baseline.len(), 8 * frame_bytes);

        for interpolation in [
            KeyframeInterpolation::Smooth,
            KeyframeInterpolation::EaseIn,
            KeyframeInterpolation::EaseOut,
        ] {
            let brightness = AnimatedScalar {
                value: 0.0,
                keyframes: vec![
                    ScalarKeyframe {
                        source_tick: Tick(0),
                        value: 0.0,
                        interpolation,
                    },
                    ScalarKeyframe {
                        source_tick: Tick(1_000_000),
                        value: 0.2,
                        interpolation: KeyframeInterpolation::Linear,
                    },
                ],
            };
            clip.video_effects = vec![brightness_contrast_node(
                true,
                brightness.clone(),
                scalar(1.0),
            )]
            .into();
            let corrected = sample(&format!(
                "format=rgba{}",
                video_color_filter(&clip).unwrap()
            ));
            assert_eq!(corrected.len(), baseline.len());

            for frame in 0..8 {
                let tick = Tick(frame as i64 * 125_000);
                let offset = frame * frame_bytes;
                let adjustment = brightness.evaluate(tick);
                for channel in 0..3 {
                    let encoded = baseline[offset + channel] as f32 / 255.0;
                    let expected = ((encoded + adjustment).clamp(0.0, 1.0) * 255.0).round() as i16;
                    assert!(
                        (i16::from(corrected[offset + channel]) - expected).abs() <= 2,
                        "{interpolation:?} frame {frame} channel {channel}: corrected={}, expected={expected}",
                        corrected[offset + channel]
                    );
                }
                assert_eq!(
                    corrected[offset + 3],
                    baseline[offset + 3],
                    "{interpolation:?} frame {frame} changed alpha"
                );
            }
        }
    }

    #[test]
    fn real_ffmpeg_equal_power_audio_crossfade_keeps_midpoint_energy() {
        let _ffmpeg_guard = real_ffmpeg_test_guard();
        let Some(root) = std::env::var_os("FFMPEG_DIR").map(PathBuf::from) else {
            return;
        };
        let ffmpeg = root.join("bin").join(if cfg!(windows) {
            "ffmpeg.exe"
        } else {
            "ffmpeg"
        });
        if !ffmpeg.exists() {
            return;
        }
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp = std::env::temp_dir();
        let video = temp.join(format!("maelstrom-xfade-video-{nonce}.mp4"));
        let left_audio = temp.join(format!("maelstrom-xfade-left-{nonce}.wav"));
        let right_audio = temp.join(format!("maelstrom-xfade-right-{nonce}.wav"));
        let output = temp.join(format!("maelstrom-xfade-output-{nonce}.mp4"));
        let filter = temp.join(format!("maelstrom-xfade-{nonce}.filter"));
        let _cleanup = TempFiles(vec![
            filter.clone(),
            video.clone(),
            left_audio.clone(),
            right_audio.clone(),
            output.clone(),
        ]);
        let status = Command::new(&ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:size=64x36:rate=24:d=4",
                "-c:v",
                "mpeg4",
                "-q:v",
                "2",
            ])
            .arg(&video)
            .status()
            .unwrap();
        assert!(status.success());
        for (path, volume) in [(&left_audio, "0.4"), (&right_audio, "0.8")] {
            let status = Command::new(&ffmpeg)
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-y",
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=1000:sample_rate=48000:duration=4",
                    "-af",
                    &format!("volume={volume}"),
                    "-c:a",
                    "pcm_s16le",
                ])
                .arg(path)
                .status()
                .unwrap();
            assert!(status.success());
        }

        let mut editor = EditorState::new(Language::English, "Real audio crossfade");
        editor.add_media_paths([video.clone(), left_audio.clone(), right_audio.clone()]);
        for media in &mut editor.media {
            media.duration = Some(Tick(4_000_000));
        }
        let video_track = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap()
            .id;
        editor
            .timeline
            .insert_clip(video_track, MediaId(1), Tick(0), Tick(2_000_000), Tick(0))
            .unwrap();
        let audio_track = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Audio)
            .unwrap()
            .id;
        let left = editor
            .timeline
            .insert_clip(
                audio_track,
                MediaId(2),
                Tick(0),
                Tick(1_000_000),
                Tick(1_000_000),
            )
            .unwrap();
        let right = editor
            .timeline
            .insert_clip(
                audio_track,
                MediaId(3),
                Tick(1_000_000),
                Tick(1_000_000),
                Tick(1_000_000),
            )
            .unwrap();
        editor
            .timeline
            .add_audio_transition(audio_track, left, right, Tick(1_000_000))
            .unwrap();
        let request = ExportRequest {
            snapshot: editor.snapshot(),
            settings: ProjectSettings {
                fps: [24, 1],
                size: [64, 36],
            },
            output: output.clone(),
            ffmpeg: ffmpeg.clone(),
            encoders: vec![H264Encoder::OpenH264],
        };
        let plan = ExportPlan::from_request(&request).unwrap();
        let (mut args, graph) = build_ffmpeg_job(&request, &plan, H264Encoder::OpenH264).unwrap();
        let encoder = args.iter().position(|arg| arg == "-c:v").unwrap();
        args[encoder + 1] = "mpeg4".to_owned();
        fs::write(&filter, graph).unwrap();
        let cancel = AtomicBool::new(false);
        let (events, _) = mpsc::channel();
        let notify: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});
        let render = run_child(
            &ffmpeg,
            &args,
            &filter,
            plan.duration,
            &cancel,
            &events,
            &notify,
        );
        assert!(render.is_ok(), "{render:?}");

        let rms = |time: &str| -> f64 {
            let audio = Command::new(&ffmpeg)
                .args(["-hide_banner", "-loglevel", "error", "-i"])
                .arg(&output)
                .args([
                    "-ss", time, "-t", "0.080", "-vn", "-ac", "1", "-ar", "48000", "-f", "f32le",
                    "pipe:1",
                ])
                .output()
                .unwrap();
            assert!(audio.status.success());
            let samples = audio
                .stdout
                .chunks_exact(4)
                .map(|bytes| f32::from_le_bytes(bytes.try_into().unwrap()) as f64)
                .collect::<Vec<_>>();
            assert!(!samples.is_empty());
            (samples.iter().map(|sample| sample * sample).sum::<f64>() / samples.len() as f64)
                .sqrt()
        };
        let before = rms("0.250");
        let middle = rms("1.000");
        let after = rms("1.750");
        assert!((1.7..=2.3).contains(&(after / before)), "{before} {after}");
        assert!(
            (0.97..=1.15).contains(&(middle / after)),
            "equal-power midpoint lost energy: before={before} middle={middle} after={after}"
        );
    }

    #[test]
    fn real_ffmpeg_vfr_video_trim_matches_preview_floor_sampling() {
        let _ffmpeg_guard = real_ffmpeg_test_guard();
        let Some(root) = std::env::var_os("FFMPEG_DIR").map(PathBuf::from) else {
            return;
        };
        let ffmpeg = root.join("bin").join(if cfg!(windows) {
            "ffmpeg.exe"
        } else {
            "ffmpeg"
        });
        if !ffmpeg.exists() {
            return;
        }
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp = std::env::temp_dir();
        let source = temp.join(format!("maelstrom-vfr-source-{nonce}.mp4"));
        let filter = temp.join(format!("maelstrom-vfr-{nonce}.filter"));
        let concat = temp.join(format!("maelstrom-vfr-{nonce}.concat"));
        let frames = ["red", "green", "blue", "yellow", "magenta"]
            .map(|color| temp.join(format!("maelstrom-vfr-{color}-{nonce}.bmp")));
        let _cleanup = TempFiles(
            std::iter::once(source.clone())
                .chain(std::iter::once(filter.clone()))
                .chain(std::iter::once(concat.clone()))
                .chain(frames.iter().cloned())
                .collect(),
        );
        for (path, color) in frames
            .iter()
            .zip(["red", "green", "blue", "yellow", "magenta"])
        {
            let status = Command::new(&ffmpeg)
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-y",
                    "-f",
                    "lavfi",
                    "-i",
                    &format!("color=c={color}:size=160x90:rate=25:d=0.04"),
                    "-frames:v",
                    "1",
                ])
                .arg(path)
                .status()
                .unwrap();
            assert!(status.success());
        }
        let concat_contents = frames
            .iter()
            .zip(["0.040", "0.070", "0.040", "0.090", "0.040"])
            .map(|(path, duration)| {
                format!(
                    "file '{}'\nduration {duration}\n",
                    path.to_string_lossy().replace('\\', "/")
                )
            })
            .collect::<String>();
        fs::write(&concat, concat_contents).unwrap();
        let status = Command::new(&ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "concat",
                "-safe",
                "0",
                "-i",
            ])
            .arg(&concat)
            .args([
                "-vf",
                "settb=1/1000,setpts='if(eq(N\\,0)\\,0\\,if(eq(N\\,1)\\,40\\,if(eq(N\\,2)\\,110\\,if(eq(N\\,3)\\,150\\,240))))'",
                "-fps_mode",
                "passthrough",
                "-enc_time_base",
                "1/1000",
                "-c:v",
                "mpeg4",
                "-q:v",
                "2",
                "-video_track_timescale",
                "1000",
            ])
            .arg(&source)
            .status()
            .unwrap();
        assert!(status.success());

        let decode_frames = |path: &Path| -> Vec<Vec<u8>> {
            let decoded = Command::new(&ffmpeg)
                .args(["-hide_banner", "-loglevel", "error", "-i"])
                .arg(path)
                .args([
                    "-fps_mode",
                    "passthrough",
                    "-f",
                    "rawvideo",
                    "-pix_fmt",
                    "rgb24",
                    "pipe:1",
                ])
                .output()
                .unwrap();
            assert!(decoded.status.success());
            const FRAME_BYTES: usize = 160 * 90 * 3;
            assert_eq!(decoded.stdout.len() % FRAME_BYTES, 0);
            decoded
                .stdout
                .chunks_exact(FRAME_BYTES)
                .map(ToOwned::to_owned)
                .collect()
        };
        let source_frames = decode_frames(&source);
        assert_eq!(
            source_frames.len(),
            5,
            "fixture lost irregular source frames"
        );
        let ffprobe = root.join("bin").join(if cfg!(windows) {
            "ffprobe.exe"
        } else {
            "ffprobe"
        });
        let pts = Command::new(ffprobe)
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "frame=best_effort_timestamp_time",
                "-of",
                "csv=p=0",
            ])
            .arg(&source)
            .output()
            .unwrap();
        assert!(pts.status.success());
        let pts_ms = String::from_utf8(pts.stdout)
            .unwrap()
            .lines()
            .map(|value| (value.parse::<f64>().unwrap() * 1_000.0).round() as i64)
            .collect::<Vec<_>>();
        assert_eq!(pts_ms, [0, 40, 110, 150, 240]);

        for fps in [[30, 1], [30_000, 1_001]] {
            let output = temp.join(format!(
                "maelstrom-vfr-output-{}-{}-{nonce}.mp4",
                fps[0], fps[1]
            ));
            let mut editor = EditorState::new(Language::English, "VFR export");
            editor.add_media_paths([source.clone()]);
            editor.media[0].duration = Some(Tick(250_000));
            let track = editor
                .timeline
                .tracks
                .iter()
                .find(|track| track.kind == TrackKind::Video)
                .unwrap()
                .id;
            editor
                .timeline
                .insert_clip(track, MediaId(1), Tick(0), Tick(140_000), Tick(100_000))
                .unwrap();
            let request = ExportRequest {
                snapshot: editor.snapshot(),
                settings: ProjectSettings {
                    fps,
                    size: [160, 90],
                },
                output: output.clone(),
                ffmpeg: ffmpeg.clone(),
                encoders: vec![H264Encoder::OpenH264],
            };
            let plan = ExportPlan::from_request(&request).unwrap();
            let (mut args, graph) =
                build_ffmpeg_job(&request, &plan, H264Encoder::OpenH264).unwrap();
            let encoder = args.iter().position(|arg| arg == "-c:v").unwrap();
            args[encoder + 1] = "mpeg4".to_owned();
            fs::write(&filter, graph).unwrap();
            let cancel = AtomicBool::new(false);
            let (events, _) = mpsc::channel();
            let notify: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});
            let render = run_child(
                &ffmpeg,
                &args,
                &filter,
                plan.duration,
                &cancel,
                &events,
                &notify,
            );
            assert!(render.is_ok(), "{render:?}");

            let output_frames = decode_frames(&output);
            let identities = output_frames
                .iter()
                .map(|frame| {
                    source_frames
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, reference)| {
                            frame
                                .iter()
                                .zip(reference.iter())
                                .map(|(a, b)| {
                                    let delta = i32::from(*a) - i32::from(*b);
                                    i64::from(delta * delta)
                                })
                                .sum::<i64>()
                        })
                        .unwrap()
                        .0
                })
                .collect::<Vec<_>>();
            assert_eq!(
                identities,
                [1, 2, 3, 3],
                "fps={fps:?}, identities={identities:?}"
            );
            let _ = fs::remove_file(output);
        }
    }

    #[test]
    fn real_ffmpeg_dip_to_black_matte_blacks_out_lower_tracks_without_saved_handles() {
        let _ffmpeg_guard = real_ffmpeg_test_guard();
        let Some(root) = std::env::var_os("FFMPEG_DIR").map(PathBuf::from) else {
            return;
        };
        let ffmpeg = root.join("bin").join(if cfg!(windows) {
            "ffmpeg.exe"
        } else {
            "ffmpeg"
        });
        if !ffmpeg.exists() {
            return;
        }
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp = std::env::temp_dir();
        let red = temp.join(format!("maelstrom-dip-red-{nonce}.mp4"));
        let blue = temp.join(format!("maelstrom-dip-blue-{nonce}.mp4"));
        let green = temp.join(format!("maelstrom-dip-green-{nonce}.mp4"));
        let output = temp.join(format!("maelstrom-dip-output-{nonce}.mp4"));
        for (path, source) in [
            (&red, "color=c=red:size=64x36:rate=24:d=3"),
            (&blue, "color=c=blue:size=64x36:rate=24:d=3"),
            (&green, "color=c=green:size=64x36:rate=24:d=3"),
        ] {
            let status = Command::new(&ffmpeg)
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-y",
                    "-f",
                    "lavfi",
                    "-i",
                    source,
                    "-c:v",
                    "mpeg4",
                    "-q:v",
                    "2",
                ])
                .arg(path)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap();
            assert!(status.success());
        }

        let mut editor = EditorState::new(Language::English, "Real dip transition");
        editor.add_media_paths([red.clone(), blue.clone(), green.clone()]);
        let lower_track = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap()
            .id;
        editor
            .timeline
            .insert_clip(
                lower_track,
                MediaId(3),
                Tick(0),
                Tick(2_000_000),
                Tick(500_000),
            )
            .unwrap();
        let track = editor.timeline.add_track(TrackKind::Video);
        let left = editor
            .timeline
            .insert_clip(track, MediaId(1), Tick(0), Tick(1_000_000), Tick(500_000))
            .unwrap();
        let right = editor
            .timeline
            .insert_clip(
                track,
                MediaId(2),
                Tick(1_000_000),
                Tick(1_000_000),
                Tick(500_000),
            )
            .unwrap();
        editor
            .timeline
            .add_video_transition_of_kind(
                track,
                left,
                right,
                Tick(1_000_000),
                0.0,
                VideoTransitionKind::DipToBlack,
            )
            .unwrap();
        let request = ExportRequest {
            snapshot: editor.snapshot(),
            settings: ProjectSettings {
                fps: [24, 1],
                size: [64, 36],
            },
            output: output.clone(),
            ffmpeg: ffmpeg.clone(),
            encoders: vec![H264Encoder::OpenH264],
        };
        assert!(
            request
                .snapshot
                .media
                .iter()
                .all(|media| media.duration.is_none())
        );
        let plan = ExportPlan::from_request(&request).unwrap();
        assert_eq!(
            plan.video_tracks[1].clips[0].input_duration,
            Tick(1_000_000)
        );
        assert_eq!(
            plan.video_tracks[1].clips[1].input_duration,
            Tick(1_000_000)
        );
        let (mut args, graph) = build_ffmpeg_job(&request, &plan, H264Encoder::OpenH264).unwrap();
        let encoder = args.iter().position(|arg| arg == "-c:v").unwrap();
        args[encoder + 1] = "mpeg4".to_owned();
        let filter = temp.join(format!("maelstrom-dip-{nonce}.filter"));
        fs::write(&filter, graph).unwrap();
        let cancel = AtomicBool::new(false);
        let (events, _) = mpsc::channel();
        let notify: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});
        let render = run_child(
            &ffmpeg,
            &args,
            &filter,
            plan.duration,
            &cancel,
            &events,
            &notify,
        );
        assert!(render.is_ok(), "{render:?}");

        let sample = |time: &str| -> [u8; 3] {
            let frame = Command::new(&ffmpeg)
                .args(["-hide_banner", "-loglevel", "error", "-ss", time, "-i"])
                .arg(&output)
                .args([
                    "-vf",
                    "format=rgb24,crop=1:1:32:18",
                    "-frames:v",
                    "1",
                    "-f",
                    "rawvideo",
                    "-pix_fmt",
                    "rgb24",
                    "pipe:1",
                ])
                .output()
                .unwrap();
            assert!(frame.status.success());
            frame.stdout[..3].try_into().unwrap()
        };
        let before = sample("0.250");
        let middle = sample("1.000");
        let after = sample("1.750");
        assert!(
            before[0] > 150 && before[1] < 70 && before[2] < 70,
            "{before:?}"
        );
        assert!(
            middle[0] < 25 && middle[1] < 25 && middle[2] < 25,
            "{middle:?}"
        );
        assert!(
            after[0] < 70 && after[1] < 70 && after[2] > 150,
            "{after:?}"
        );

        for path in [&filter, &red, &blue, &green, &output] {
            let _ = fs::remove_file(path);
        }
    }

    #[test]
    fn real_ffmpeg_cross_dissolve_has_red_purple_blue_timing() {
        let _ffmpeg_guard = real_ffmpeg_test_guard();
        let Some(root) = std::env::var_os("FFMPEG_DIR").map(PathBuf::from) else {
            return;
        };
        let ffmpeg = root.join("bin").join(if cfg!(windows) {
            "ffmpeg.exe"
        } else {
            "ffmpeg"
        });
        if !ffmpeg.exists() {
            return;
        }
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp = std::env::temp_dir();
        let red = temp.join(format!("maelstrom-transition-red-{nonce}.mp4"));
        let blue = temp.join(format!("maelstrom-transition-blue-{nonce}.mp4"));
        let output = temp.join(format!("maelstrom-transition-output-{nonce}.mp4"));
        for (path, source) in [
            (&red, "color=c=red:size=64x36:rate=24:d=3"),
            (&blue, "color=c=blue:size=64x36:rate=24:d=3"),
        ] {
            let status = Command::new(&ffmpeg)
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-y",
                    "-f",
                    "lavfi",
                    "-i",
                    source,
                    "-c:v",
                    "mpeg4",
                    "-q:v",
                    "2",
                ])
                .arg(path)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap();
            assert!(status.success());
        }

        let mut editor = EditorState::new(Language::English, "Real transition");
        editor.add_media_paths([red.clone(), blue.clone()]);
        let track = editor
            .timeline
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap()
            .id;
        let left = editor
            .timeline
            .insert_clip(track, MediaId(1), Tick(0), Tick(1_000_000), Tick(500_000))
            .unwrap();
        let right = editor
            .timeline
            .insert_clip(
                track,
                MediaId(2),
                Tick(1_000_000),
                Tick(1_000_000),
                Tick(500_000),
            )
            .unwrap();
        editor
            .timeline
            .add_video_transition(track, left, right, Tick(1_000_000), 0.0)
            .unwrap();
        let mut snapshot = editor.snapshot();
        for media in &mut snapshot.media {
            media.duration = Some(Tick(3_000_000));
        }
        let request = ExportRequest {
            snapshot,
            settings: ProjectSettings {
                fps: [24, 1],
                size: [64, 36],
            },
            output: output.clone(),
            ffmpeg: ffmpeg.clone(),
            encoders: vec![H264Encoder::OpenH264],
        };
        let plan = ExportPlan::from_request(&request).unwrap();
        let (mut args, graph) = build_ffmpeg_job(&request, &plan, H264Encoder::OpenH264).unwrap();
        let encoder = args.iter().position(|arg| arg == "-c:v").unwrap();
        args[encoder + 1] = "mpeg4".to_owned();
        let filter = temp.join(format!("maelstrom-transition-{nonce}.filter"));
        fs::write(&filter, graph).unwrap();
        let cancel = AtomicBool::new(false);
        let (events, _) = mpsc::channel();
        let notify: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});
        let render = run_child(
            &ffmpeg,
            &args,
            &filter,
            plan.duration,
            &cancel,
            &events,
            &notify,
        );
        assert!(render.is_ok(), "{render:?}");

        let sample = |time: &str| -> [u8; 3] {
            let frame = Command::new(&ffmpeg)
                .args(["-hide_banner", "-loglevel", "error", "-ss", time, "-i"])
                .arg(&output)
                .args([
                    "-vf",
                    "format=rgb24,crop=1:1:32:18",
                    "-frames:v",
                    "1",
                    "-f",
                    "rawvideo",
                    "-pix_fmt",
                    "rgb24",
                    "pipe:1",
                ])
                .output()
                .unwrap();
            assert!(frame.status.success());
            frame.stdout[..3].try_into().unwrap()
        };
        let before = sample("0.250");
        let middle = sample("1.000");
        let after = sample("1.750");
        assert!(
            before[0] > 150 && before[1] < 70 && before[2] < 70,
            "{before:?}"
        );
        assert!(
            middle[0] > 70 && middle[1] < 70 && middle[2] > 70,
            "{middle:?}"
        );
        assert!(
            after[0] < 70 && after[1] < 70 && after[2] > 150,
            "{after:?}"
        );

        for path in [&filter, &red, &blue, &output] {
            let _ = fs::remove_file(path);
        }
    }

    #[test]
    fn real_ffmpeg_renders_new_transition_families_with_authored_curve() {
        let _ffmpeg_guard = real_ffmpeg_test_guard();
        let Some(root) = std::env::var_os("FFMPEG_DIR").map(PathBuf::from) else {
            return;
        };
        let ffmpeg = root.join("bin").join(if cfg!(windows) {
            "ffmpeg.exe"
        } else {
            "ffmpeg"
        });
        if !ffmpeg.exists() {
            return;
        }
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp = std::env::temp_dir();
        let red = temp.join(format!("maelstrom-family-red-{nonce}.mp4"));
        let blue = temp.join(format!("maelstrom-family-blue-{nonce}.mp4"));
        for (path, source) in [
            (&red, "color=c=red:size=64x36:rate=24:d=3"),
            (&blue, "color=c=blue:size=64x36:rate=24:d=3"),
        ] {
            let status = Command::new(&ffmpeg)
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-y",
                    "-f",
                    "lavfi",
                    "-i",
                    source,
                    "-c:v",
                    "mpeg4",
                    "-q:v",
                    "2",
                ])
                .arg(path)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap();
            assert!(status.success());
        }

        for kind in [
            VideoTransitionKind::FilmDissolve,
            VideoTransitionKind::DipToWhite,
            VideoTransitionKind::WipeLeft,
            VideoTransitionKind::SlideFromLeft,
            VideoTransitionKind::PushFromLeft,
            VideoTransitionKind::PushFromRight,
            VideoTransitionKind::PushFromTop,
            VideoTransitionKind::PushFromBottom,
        ] {
            let mut editor = EditorState::new(Language::English, "Real transition families");
            editor.add_media_paths([red.clone(), blue.clone()]);
            let track = editor
                .timeline
                .tracks
                .iter()
                .find(|track| track.kind == TrackKind::Video)
                .unwrap()
                .id;
            let left = editor
                .timeline
                .insert_clip(track, MediaId(1), Tick(0), Tick(1_000_000), Tick(500_000))
                .unwrap();
            let right = editor
                .timeline
                .insert_clip(
                    track,
                    MediaId(2),
                    Tick(1_000_000),
                    Tick(1_000_000),
                    Tick(500_000),
                )
                .unwrap();
            editor
                .timeline
                .add_video_transition_of_kind(track, left, right, Tick(1_000_000), 0.5, kind)
                .unwrap();
            let mut snapshot = editor.snapshot();
            for media in &mut snapshot.media {
                media.duration = Some(Tick(3_000_000));
            }
            let suffix = format!("{kind:?}");
            let output = temp.join(format!("maelstrom-family-{suffix}-{nonce}.mp4"));
            let filter = temp.join(format!("maelstrom-family-{suffix}-{nonce}.filter"));
            let request = ExportRequest {
                snapshot,
                settings: ProjectSettings {
                    fps: [24, 1],
                    size: [64, 36],
                },
                output: output.clone(),
                ffmpeg: ffmpeg.clone(),
                encoders: vec![H264Encoder::OpenH264],
            };
            let plan = ExportPlan::from_request(&request).unwrap();
            let (mut args, graph) =
                build_ffmpeg_job(&request, &plan, H264Encoder::OpenH264).unwrap();
            let encoder = args.iter().position(|arg| arg == "-c:v").unwrap();
            args[encoder + 1] = "mpeg4".to_owned();
            fs::write(&filter, graph).unwrap();
            let cancel = AtomicBool::new(false);
            let (events, _) = mpsc::channel();
            let notify: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});
            let render = run_child(
                &ffmpeg,
                &args,
                &filter,
                plan.duration,
                &cancel,
                &events,
                &notify,
            );
            assert!(render.is_ok(), "{kind:?}: {render:?}");
            assert!(output.metadata().is_ok_and(|metadata| metadata.len() > 0));
            if let Some((blue_at, red_at)) = match kind {
                VideoTransitionKind::PushFromLeft => Some(((16, 18), (56, 18))),
                VideoTransitionKind::PushFromRight => Some(((48, 18), (8, 18))),
                VideoTransitionKind::PushFromTop => Some(((32, 7), (32, 31))),
                VideoTransitionKind::PushFromBottom => Some(((32, 29), (32, 5))),
                _ => None,
            } {
                let sample = |(x, y): (u32, u32)| -> [u8; 3] {
                    let frame = Command::new(&ffmpeg)
                        .args(["-hide_banner", "-loglevel", "error", "-ss", "1.000", "-i"])
                        .arg(&output)
                        .args([
                            "-vf",
                            &format!("format=rgb24,crop=1:1:{x}:{y}"),
                            "-frames:v",
                            "1",
                            "-f",
                            "rawvideo",
                            "-pix_fmt",
                            "rgb24",
                            "pipe:1",
                        ])
                        .output()
                        .unwrap();
                    assert!(
                        frame.status.success(),
                        "{kind:?}: {}",
                        String::from_utf8_lossy(&frame.stderr)
                    );
                    frame.stdout[..3].try_into().unwrap()
                };
                let blue = sample(blue_at);
                let red = sample(red_at);
                assert!(
                    blue[2] > 140 && blue[0] < 90,
                    "{kind:?} incoming sample {blue_at:?} was not blue: {blue:?}"
                );
                assert!(
                    red[0] > 140 && red[2] < 90,
                    "{kind:?} outgoing sample {red_at:?} was not red: {red:?}"
                );
            }
            let _ = fs::remove_file(filter);
            let _ = fs::remove_file(output);
        }

        let _ = fs::remove_file(red);
        let _ = fs::remove_file(blue);
    }

    #[test]
    fn real_ffmpeg_preserves_transformed_layer_order_in_output_pixels() {
        let _ffmpeg_guard = real_ffmpeg_test_guard();
        let Some(root) = std::env::var_os("FFMPEG_DIR").map(PathBuf::from) else {
            return;
        };
        let ffmpeg = root.join("bin").join(if cfg!(windows) {
            "ffmpeg.exe"
        } else {
            "ffmpeg"
        });
        if !ffmpeg.exists() {
            return;
        }
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp = std::env::temp_dir();
        let base = temp.join(format!("maelstrom-export-base-{nonce}.mp4"));
        let upper = temp.join(format!("maelstrom-export-upper-{nonce}.mp4"));
        let output = temp.join(format!("maelstrom-export-composite-{nonce}.mp4"));
        for (path, source) in [
            (&base, "color=c=red:size=320x180:rate=24:d=1"),
            (&upper, "color=c=blue:size=80x80:rate=24:d=1"),
        ] {
            let status = Command::new(&ffmpeg)
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-y",
                    "-f",
                    "lavfi",
                    "-i",
                    source,
                ])
                .args(["-t", "1", "-c:v", "mpeg4", "-q:v", "2"])
                .arg(path)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap();
            assert!(status.success());
        }

        let mut editor = EditorState::new(Language::English, "Pixel parity");
        editor.add_media_paths([base.clone(), upper.clone()]);
        let tracks = editor
            .timeline
            .tracks
            .iter()
            .filter(|track| track.kind == TrackKind::Video)
            .map(|track| track.id)
            .take(2)
            .collect::<Vec<_>>();
        editor
            .timeline
            .insert_clip(tracks[0], MediaId(1), Tick(0), Tick(1_000_000), Tick(0))
            .unwrap();
        let upper_clip = editor
            .timeline
            .insert_clip(tracks[1], MediaId(2), Tick(0), Tick(1_000_000), Tick(0))
            .unwrap();
        editor
            .timeline
            .set_clip_transform(
                upper_clip,
                ClipTransform {
                    scale_x: 0.8,
                    scale_y: 0.8,
                    rotation_degrees: 30.0,
                    sizing_mode: ClipSizingMode::Original,
                    ..Default::default()
                },
            )
            .unwrap();
        let request = ExportRequest {
            snapshot: editor.snapshot(),
            settings: ProjectSettings {
                fps: [24, 1],
                size: [320, 180],
            },
            output: output.clone(),
            ffmpeg: ffmpeg.clone(),
            encoders: vec![H264Encoder::OpenH264],
        };
        let plan = ExportPlan::from_request_with_probe(&request, |path| {
            Ok(MediaProbe {
                source_size: Some(if path == base {
                    PixelSize::new(320, 180)
                } else {
                    PixelSize::new(80, 80)
                }),
                has_audio: false,
                ..Default::default()
            })
        })
        .unwrap();
        let (mut args, graph) = build_ffmpeg_job(&request, &plan, H264Encoder::OpenH264).unwrap();
        let encoder = args.iter().position(|arg| arg == "-c:v").unwrap();
        args[encoder + 1] = "mpeg4".to_owned();
        let filter = temp.join(format!("maelstrom-export-composite-{nonce}.filter"));
        fs::write(&filter, &graph).unwrap();
        let cancel = AtomicBool::new(false);
        let (events, _) = mpsc::channel();
        let notify: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});
        let render = run_child(
            &ffmpeg,
            &args,
            &filter,
            plan.duration,
            &cancel,
            &events,
            &notify,
        );

        let sample = |x: u32, y: u32| -> Vec<u8> {
            Command::new(&ffmpeg)
                .args(["-hide_banner", "-loglevel", "error", "-ss", "0.5", "-i"])
                .arg(&output)
                .args([
                    "-vf",
                    &format!("format=rgb24,crop=1:1:{x}:{y}"),
                    "-frames:v",
                    "1",
                    "-f",
                    "rawvideo",
                    "-pix_fmt",
                    "rgb24",
                    "pipe:1",
                ])
                .output()
                .unwrap()
                .stdout
        };
        let corner = sample(10, 10);
        let center = sample(160, 90);
        for path in [&filter, &base, &upper, &output] {
            let _ = fs::remove_file(path);
        }
        assert!(render.is_ok(), "{render:?}");
        assert!(
            corner.len() >= 3 && corner[0] > 150 && corner[2] < 100,
            "corner={corner:?}, center={center:?}, graph={graph}"
        );
        assert!(
            center.len() >= 3 && center[2] > 120 && center[0] < 120,
            "{center:?}"
        );
    }
}
