use std::ptr;

use nle_timeline::{
    BrightnessContrastEffect, Clip, ClipData, ClipId, Fade, KeyframeInterpolation, MediaId,
    ScalarKeyframe, Tick, Timeline, VideoEffectId, VideoEffectKind, VideoEffectNode,
};

fn timeline_with_clips() -> Timeline {
    let mut timeline = Timeline::new_default();
    let track = timeline.tracks[0].id;
    timeline
        .insert_clip(track, MediaId(1), Tick(0), Tick(10), Tick(0))
        .unwrap();
    timeline
        .insert_clip(track, MediaId(1), Tick(20), Tick(10), Tick(0))
        .unwrap();
    timeline
}

fn shared(a: &Clip, b: &Clip) -> bool {
    ptr::eq(&**a, &**b)
}

#[test]
fn snapshot_and_canonical_restore_share_all_records() {
    let timeline = timeline_with_clips();
    let snapshot = timeline.snapshot();
    assert!(shared(
        &snapshot.tracks[0].clips[0],
        &timeline.tracks[0].clips[0]
    ));
    assert!(shared(
        &snapshot.tracks[0].clips[1],
        &timeline.tracks[0].clips[1]
    ));
    let restored = Timeline::from_snapshot(snapshot).unwrap();
    assert!(shared(
        &restored.tracks[0].clips[0],
        &timeline.tracks[0].clips[0]
    ));
    assert!(shared(
        &restored.tracks[0].clips[1],
        &timeline.tracks[0].clips[1]
    ));
}

#[test]
fn scalar_and_nested_mutations_detach_only_touched_record() {
    let mut timeline = timeline_with_clips();
    timeline.tracks[0].clips[1]
        .video_effects
        .push(VideoEffectNode {
            id: VideoEffectId(1),
            enabled: true,
            kind: VideoEffectKind::BrightnessContrast(BrightnessContrastEffect::default()),
        });
    if let VideoEffectKind::BrightnessContrast(effect) =
        &mut timeline.tracks[0].clips[1].video_effects[0].kind
    {
        effect.brightness.keyframes.push(ScalarKeyframe {
            source_tick: Tick(1),
            value: 0.5,
            interpolation: KeyframeInterpolation::Hold,
        });
    }
    let snapshot = timeline.snapshot();
    let saved_json = serde_json::to_value(&snapshot).unwrap();
    timeline.tracks[0].clips[0].gain_db = 3.0;
    assert!(!shared(
        &timeline.tracks[0].clips[0],
        &snapshot.tracks[0].clips[0]
    ));
    assert!(shared(
        &timeline.tracks[0].clips[1],
        &snapshot.tracks[0].clips[1]
    ));

    if let VideoEffectKind::BrightnessContrast(effect) =
        &mut timeline.tracks[0].clips[1].video_effects[0].kind
    {
        effect.brightness.keyframes[0].value = 0.75;
    }
    assert!(!shared(
        &timeline.tracks[0].clips[1],
        &snapshot.tracks[0].clips[1]
    ));
    assert_eq!(serde_json::to_value(&snapshot).unwrap(), saved_json);
    assert_eq!(timeline.tracks[0].clips[0].gain_db, 3.0);
    let VideoEffectKind::BrightnessContrast(effect) =
        &timeline.tracks[0].clips[1].video_effects[0].kind
    else {
        panic!("expected color effect")
    };
    assert_eq!(effect.brightness.keyframes[0].value, 0.75);
}

#[test]
fn undo_redo_preserve_values() {
    let mut timeline = timeline_with_clips();
    let before = timeline.snapshot();
    let mut after = before.clone();
    after.tracks[0].clips[0].make_mut().gain_db = 7.0;
    let mut history = nle_timeline::UndoStack::default();
    assert!(history.record(&before, &after));
    timeline = Timeline::from_snapshot(after).unwrap();
    assert!(history.undo(&mut timeline));
    assert_eq!(timeline.snapshot(), before);
    assert!(history.redo(&mut timeline));
    assert_eq!(timeline.tracks[0].clips[0].gain_db, 7.0);
}

#[test]
fn noncanonical_normalization_detaches_only_affected_clip() {
    let timeline = timeline_with_clips();
    let mut snapshot = timeline.snapshot();
    snapshot.tracks[0].clips[0].fade_in = Fade {
        duration: Tick(99),
        curve: 2.0,
    };
    let invalid_before = snapshot.clone();
    let restored = Timeline::from_snapshot(snapshot).unwrap();
    assert!(!shared(
        &restored.tracks[0].clips[0],
        &invalid_before.tracks[0].clips[0]
    ));
    assert!(shared(
        &restored.tracks[0].clips[1],
        &invalid_before.tracks[0].clips[1]
    ));
    assert_eq!(restored.tracks[0].clips[0].fade_in.duration, Tick(10));
    assert_eq!(restored.tracks[0].clips[0].fade_in.curve, 1.0);
    assert_eq!(invalid_before.tracks[0].clips[0].fade_in.duration, Tick(99));
    assert_eq!(invalid_before.tracks[0].clips[0].fade_in.curve, 2.0);
}

#[test]
fn clip_json_is_flat_and_legacy_defaults_apply() {
    let data = ClipData {
        id: ClipId(1),
        media: MediaId(1),
        track_id: nle_timeline::TrackId(1),
        link_id: None,
        enabled: true,
        start: Tick(0),
        duration: Tick(10),
        source_in: Tick(0),
        gain_db: 0.0,
        gain_left_db: 0.0,
        gain_right_db: 0.0,
        effects: Vec::new(),
        video_effects: Vec::new(),
        transform: nle_timeline::ClipTransform::default(),
        fade_in: Fade::default(),
        fade_out: Fade::default(),
    };
    let clip = Clip::new(data.clone());
    let json = serde_json::to_value(&clip).unwrap();
    assert_eq!(json, serde_json::to_value(&data).unwrap());
    assert!(json.get("0").is_none());
    assert!(json.get("id").is_some());
    let legacy = serde_json::json!({"id":1,"media":1,"track_id":1,"start":0,"duration":10,"source_in":0,"fade_in":{"duration":0,"curve":0.0},"fade_out":{"duration":0,"curve":0.0}});
    let clip: Clip = serde_json::from_value(legacy).unwrap();
    assert!(clip.enabled);
    assert!(clip.effects.is_empty());
    assert_eq!(
        serde_json::from_value::<Clip>(json).unwrap(),
        Clip::new(data)
    );
}

#[test]
fn equal_records_are_distinct_and_nan_is_not_equal() {
    let a = timeline_with_clips().snapshot().tracks[0].clips[0].clone();
    let b = a.clone();
    assert!(shared(&a, &b));
    let c = Clip::new((*a).clone());
    assert_eq!(a, c);
    assert!(!shared(&a, &c));
    let mut nan = (*a).clone();
    nan.gain_db = f32::NAN;
    let nan = Clip::new(nan);
    assert_ne!(nan, nan.clone());
    assert_ne!(nan, Clip::new((*nan).clone()));
}

#[test]
fn dense_move_and_probe_detach_only_changed_records_and_keep_history() {
    let mut snapshot = timeline_with_clips().snapshot();
    let template = (*snapshot.tracks[0].clips[0]).clone();
    snapshot.tracks[0].clips = (1..=50_000)
        .map(|id| {
            Clip::new(ClipData {
                id: ClipId(id),
                start: Tick(i64::from(id - 1) * 20),
                ..template.clone()
            })
        })
        .collect();
    let mut timeline = Timeline::from_snapshot(snapshot).unwrap();
    let before = timeline.snapshot();
    for (old, live) in before.tracks[0].clips.iter().zip(&timeline.tracks[0].clips) {
        assert!(shared(old, live));
    }
    timeline
        .move_clip_with_link(ClipId(25_000), Tick(-499_970), false)
        .unwrap();
    assert_eq!(
        timeline
            .reconcile_provisional_media_duration(MediaId(1), &[ClipId(50_000)], Tick(15))
            .unwrap(),
        1
    );
    for old in &before.tracks[0].clips {
        let live = timeline.clip(old.id).unwrap();
        assert_eq!(
            shared(old, live),
            old.id != ClipId(25_000) && old.id != ClipId(50_000)
        );
        assert_eq!(old.start, Tick(i64::from(old.id.0 - 1) * 20));
        assert_eq!(old.duration, Tick(10));
    }
    let after = timeline.snapshot();
    let mut history = nle_timeline::UndoStack::default();
    assert!(history.record_current(&before, &timeline));
    assert!(history.undo(&mut timeline));
    assert_eq!(timeline.snapshot(), before);
    assert!(history.redo(&mut timeline));
    assert_eq!(timeline.snapshot(), after);
}

#[test]
fn equal_independent_records_and_noop_setters_preserve_redo() {
    let mut timeline = timeline_with_clips();
    let clip = timeline.tracks[0].clips[0].id;
    let before = timeline.snapshot();
    timeline.set_clip_enabled(clip, false, false).unwrap();
    let mut history = nle_timeline::UndoStack::default();
    assert!(history.record_current(&before, &timeline));
    assert!(history.undo(&mut timeline));
    let undo = timeline.snapshot();
    let transform = timeline.clip(clip).unwrap().transform;
    timeline.set_clip_transform(clip, transform).unwrap();
    assert!(timeline.set_audio_gain(clip, 1.0).is_err());
    assert!(shared(
        &undo.tracks[0].clips[0],
        timeline.clip(clip).unwrap()
    ));
    let independent: nle_timeline::TimelineSnapshot =
        serde_json::from_value(serde_json::to_value(&undo).unwrap()).unwrap();
    assert!(!shared(
        &undo.tracks[0].clips[0],
        &independent.tracks[0].clips[0]
    ));
    assert!(!history.record_current(&independent, &timeline));
    assert!(history.redo(&mut timeline));
    assert!(!timeline.clip(clip).unwrap().enabled);
}
