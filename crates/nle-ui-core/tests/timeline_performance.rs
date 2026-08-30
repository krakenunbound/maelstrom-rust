//! Release-only CPU layout/culling evidence for the retained timeline UI path.
//!
//! Run with:
//! `cargo test -p nle-ui-core --test timeline_performance --release -- --ignored --nocapture`

use std::{
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

use egui::{Color32, Pos2, Rect, Vec2};
use nle_decode::{AccelerationPreference, DecodeEvent, DecodeRequest, MonitorDecoder};
use nle_timeline::{Clip, ClipData, Fade, MediaId, Tick, Timeline, TimelineSnapshot, TrackKind};
use nle_ui_core::{EditorState, Language, TimelineCanvas, show_editor_with_timeline_canvas};

const CLIP_COUNT: u32 = 50_000;
const FRAME_COUNT: usize = 120;
const WARMUP_FRAMES: usize = 8;
const SCREEN_SIZE: Vec2 = Vec2::new(1_920.0, 1_080.0);
const COMBINED_CLIP_COUNT: u32 = 20_000;

/// This intentionally records only native timeline primitives. It avoids an egui GPU backend
/// while exercising the public editor CPU frame and the exact culling/banding code that feeds the
/// native renderer. GPU upload, tessellation, execution and presentation need separate evidence.
#[derive(Default)]
struct CountingTimelineCanvas {
    frame_solids: usize,
    frame_textures: usize,
    max_primitives: usize,
}

impl CountingTimelineCanvas {
    fn finish_frame(&mut self) {
        self.max_primitives = self
            .max_primitives
            .max(self.frame_solids.saturating_add(self.frame_textures));
        self.frame_solids = 0;
        self.frame_textures = 0;
    }
}

impl TimelineCanvas for CountingTimelineCanvas {
    fn begin(&mut self, _ui: &mut egui::Ui, _canvas_rect: Rect) {}

    fn solid_rect(&mut self, _rect: Rect, _color: Color32) {
        self.frame_solids += 1;
    }

    fn texture_rect(
        &mut self,
        _rect: Rect,
        _native_texture_id: u64,
        _fallback_texture: egui::TextureId,
        _uv: Rect,
        _tint: Color32,
    ) {
        self.frame_textures += 1;
    }
}

fn fifty_thousand_clips() -> Timeline {
    let mut snapshot = Timeline::new_default().snapshot();
    let video_track = snapshot
        .tracks
        .iter_mut()
        .find(|track| track.kind == TrackKind::Video)
        .expect("default timeline includes video tracks");
    video_track.clips = (1..=CLIP_COUNT)
        .map(|id| {
            Clip::new(ClipData {
                id: nle_timeline::ClipId(id),
                media: MediaId(1),
                track_id: video_track.id,
                link_id: None,
                enabled: true,
                // One millisecond clip followed by one millisecond of empty timeline. This keeps
                // the source nonoverlapping and makes the wide view a genuine 50,000-clip case.
                start: Tick(i64::from(id - 1) * 2_000),
                duration: Tick(1_000),
                source_in: Tick(0),
                gain_db: 0.0,
                gain_left_db: 0.0,
                gain_right_db: 0.0,
                effects: Vec::new(),
                video_effects: Vec::new(),
                transform: nle_timeline::ClipTransform::default(),
                fade_in: Fade::default(),
                fade_out: Fade::default(),
            })
        })
        .collect();
    Timeline::from_snapshot(TimelineSnapshot {
        tracks: snapshot.tracks,
        titles: snapshot.titles,
        transitions: snapshot.transitions,
        audio_transitions: snapshot.audio_transitions,
    })
    .expect("generated clips are ordered and nonoverlapping")
}

fn add_twenty_thousand_bars_behind_real_media(state: &mut EditorState) {
    let mut snapshot = state.timeline.snapshot();
    let max_clip_id = snapshot
        .tracks
        .iter()
        .flat_map(|track| &track.clips)
        .map(|clip| clip.id.0)
        .max()
        .unwrap_or(0);
    let stress_track = snapshot
        .tracks
        .iter_mut()
        .filter(|track| track.kind == TrackKind::Video)
        .nth(1)
        .expect("default timeline includes a second video track");
    let start = state.timeline_end().0.saturating_add(1_000_000);
    stress_track.clips = (0..COMBINED_CLIP_COUNT)
        .map(|index| {
            Clip::new(ClipData {
                id: nle_timeline::ClipId(max_clip_id + index + 1),
                media: MediaId(1),
                track_id: stress_track.id,
                link_id: None,
                enabled: true,
                start: Tick(start + i64::from(index) * 2_000),
                duration: Tick(1_000),
                source_in: Tick(0),
                gain_db: 0.0,
                gain_left_db: 0.0,
                gain_right_db: 0.0,
                effects: Vec::new(),
                video_effects: Vec::new(),
                transform: nle_timeline::ClipTransform::default(),
                fade_in: Fade::default(),
                fade_out: Fade::default(),
            })
        })
        .collect();
    state.timeline = Timeline::from_snapshot(TimelineSnapshot {
        tracks: snapshot.tracks,
        titles: snapshot.titles,
        transitions: snapshot.transitions,
        audio_transitions: snapshot.audio_transitions,
    })
    .expect("combined acceptance bars are ordered and nonoverlapping");
    state.timeline_view_start = Tick(0);
    state.timeline_view_span = Tick(state.timeline_end().0.max(1));
}

fn frame(
    context: &egui::Context,
    state: &mut EditorState,
    canvas: &mut CountingTimelineCanvas,
) -> Duration {
    let started = Instant::now();
    let _ = context.run_ui(
        egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, SCREEN_SIZE)),
            ..Default::default()
        },
        |ui| show_editor_with_timeline_canvas(ui, state, canvas),
    );
    let elapsed = started.elapsed();
    canvas.finish_frame();
    elapsed
}

fn percentile(mut values: Vec<Duration>, percentile: f64) -> Duration {
    values.sort_unstable();
    let index = ((values.len() - 1) as f64 * percentile).ceil() as usize;
    values[index]
}

fn measure_frames(
    context: &egui::Context,
    state: &mut EditorState,
    canvas: &mut CountingTimelineCanvas,
) -> (Duration, Duration, usize) {
    for _ in 0..WARMUP_FRAMES {
        frame(context, state, canvas);
    }
    canvas.max_primitives = 0;
    let samples: Vec<Duration> = (0..FRAME_COUNT)
        .map(|_| frame(context, state, canvas))
        .collect();
    (
        percentile(samples.clone(), 0.50),
        percentile(samples, 0.95),
        canvas.max_primitives,
    )
}

#[test]
#[ignore = "release performance evidence; run explicitly with --release -- --ignored --nocapture"]
fn fifty_thousand_clip_editor_cpu_evidence() {
    let context = egui::Context::default();
    let mut state = EditorState::new(Language::English, "50k performance evidence");
    state.timeline = fifty_thousand_clips();
    state.show_video_thumbnails = false;
    state.show_audio_waveforms = false;
    let mut canvas = CountingTimelineCanvas::default();

    // The default ~252 s span covers the complete workload. Each source clip is sub-pixel, so
    // cache banding must keep work bounded by display width, not by source clip count.
    let (wide_p50, wide_p95, wide_primitives) = measure_frames(&context, &mut state, &mut canvas);
    assert!(
        wide_primitives < 2_500,
        "zoomed-out native primitives must be display-bounded, got {wide_primitives}"
    );

    // Fifty individual clips can be visible at this detail level; all other 49,950 must be
    // skipped before drawing.
    state.timeline_view_start = Tick(25_000_000);
    state.timeline_view_span = Tick(100_000);
    let (detail_p50, detail_p95, detail_primitives) =
        measure_frames(&context, &mut state, &mut canvas);
    assert!(
        detail_primitives < 300,
        "zoomed-in native primitives must be viewport-culled, got {detail_primitives}"
    );

    // Playback/scrubbing mutates the clock without touching the structural TimelineCache. This
    // separately records that hot UI-path cost after the cache was warmed above.
    let mut playhead_samples = Vec::with_capacity(FRAME_COUNT);
    for index in 0..FRAME_COUNT {
        state.set_playhead(Tick(25_000_000 + i64::try_from(index).unwrap() * 700));
        playhead_samples.push(frame(&context, &mut state, &mut canvas));
    }
    let playhead_p50 = percentile(playhead_samples.clone(), 0.50);
    let playhead_p95 = percentile(playhead_samples, 0.95);

    println!(
        "timeline 50k CPU evidence: wide p50={wide_p50:?} p95={wide_p95:?} primitives={wide_primitives}; \
         detail p50={detail_p50:?} p95={detail_p95:?} primitives={detail_primitives}; \
         playhead p50={playhead_p50:?} p95={playhead_p95:?}"
    );

    // Debug instrumentation has intentionally different codegen and is useful only for checking
    // correctness here. CI/release evidence enforces the product budget.
    if !cfg!(debug_assertions) {
        assert!(
            wide_p95 < Duration::from_millis(8),
            "wide p95 exceeded 8 ms"
        );
        assert!(
            detail_p95 < Duration::from_millis(8),
            "detail p95 exceeded 8 ms"
        );
        assert!(
            playhead_p95 < Duration::from_millis(2),
            "playhead CPU p95 exceeded 2 ms"
        );
    }
}

#[test]
#[ignore = "release real-media foundation evidence; set MAELSTROM_TEST_MEDIA and run explicitly"]
fn real_h264_scrub_stays_responsive_with_twenty_thousand_bars() {
    let media_path = PathBuf::from(
        std::env::var_os("MAELSTROM_TEST_MEDIA")
            .expect("MAELSTROM_TEST_MEDIA must identify the long H.264 acceptance clip"),
    );
    assert!(
        media_path.is_file(),
        "acceptance media is missing: {media_path:?}"
    );

    let context = egui::Context::default();
    let mut state = EditorState::new(Language::English, "combined foundation evidence");
    state.add_media_paths([media_path.clone()]);
    assert!(state.insert_media_at(1, Tick(0)));
    add_twenty_thousand_bars_behind_real_media(&mut state);
    state.show_video_thumbnails = false;
    state.show_audio_waveforms = false;
    let total_bars: usize = state
        .timeline
        .tracks
        .iter()
        .map(|track| track.clips.len())
        .sum();
    assert_eq!(total_bars, COMBINED_CLIP_COUNT as usize + 2);

    let decoder = MonitorDecoder::new_with_notifier_and_cache_bytes(|| {}, 64 * 1024 * 1024);
    let mut canvas = CountingTimelineCanvas::default();
    for _ in 0..WARMUP_FRAMES {
        frame(&context, &mut state, &mut canvas);
    }
    canvas.max_primitives = 0;

    let mut samples = Vec::with_capacity(FRAME_COUNT);
    let mut latest_request_id = 0;
    let mut latest_source_tick = 0;
    for index in 0..FRAME_COUNT {
        // Alternate across a long-GOP span so the sticky decoder is continuously coalescing
        // forward and backward targets while the UI paints all 20k bars.
        latest_request_id = index as u64 + 1;
        let timeline_tick = if index % 2 == 0 {
            12_000_000
        } else {
            3_000_000
        };
        let input_started = Instant::now();
        state.set_playhead(Tick(timeline_tick));
        let _ = frame(&context, &mut state, &mut canvas);
        samples.push(input_started.elapsed());
        let target = state
            .playback_target()
            .expect("scrub target remains inside the real video clip");
        latest_source_tick = target.source_tick.0;
        decoder
            .request(DecodeRequest {
                project_epoch: 1,
                cache_epoch: 1,
                request_id: latest_request_id,
                media_id: target.media_id,
                path: target.path.to_path_buf(),
                source_tick: latest_source_tick,
                width: 320,
                height: 180,
                is_scrubbing: true,
                prewarm_scrub_workers: false,
                high_quality_scaling: true,
                progressive_scrub_frames: false,
                source_frame_duration_tick: None,
                acceleration: AccelerationPreference::Software,
            })
            .expect("monitor decoder remains available");
    }

    let p50 = percentile(samples.clone(), 0.50);
    let p95 = percentile(samples, 0.95);
    assert!(
        canvas.max_primitives < 2_500,
        "combined wide view must stay display-bounded, got {} primitives",
        canvas.max_primitives
    );
    assert!(
        p95 < Duration::from_millis(2),
        "combined scrub UI p95 exceeded 2 ms: {p95:?}"
    );

    let deadline = Instant::now() + Duration::from_secs(15);
    let completed = loop {
        match decoder.try_recv().expect("monitor decoder event channel") {
            Some(DecodeEvent::Frame(frame))
                if frame.request_id == latest_request_id
                    && (latest_source_tick - frame.source_tick).abs() <= 100_000 =>
            {
                break frame;
            }
            Some(DecodeEvent::Error(error)) if error.request_id == latest_request_id => {
                panic!("latest real-media scrub request failed: {}", error.message)
            }
            _ if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            _ => panic!("latest real-media scrub request did not complete within 15 seconds"),
        }
    };
    assert_eq!(completed.media_id, 1);
    assert!(!completed.rgba.is_empty());
    println!(
        "combined foundation evidence: bars={total_bars}, UI p50={p50:?}, p95={p95:?}, primitives={}, decoded={}us via {:?}",
        canvas.max_primitives, completed.source_tick, completed.backend
    );
}
