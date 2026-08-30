use nle_timeline::{MediaId, Tick, Timeline, TimelineError};

#[test]
fn distant_move_collision_is_rejected_without_mutation() {
    for (source_index, delta) in [(3, -58), (0, 42)] {
        let mut timeline = Timeline::new_default();
        let track = timeline.tracks[0].id;
        let clips = [0, 20, 40, 60].map(|start| {
            timeline
                .insert_clip(track, MediaId(1), Tick(start), Tick(10), Tick(0))
                .unwrap()
        });
        let before = timeline.snapshot();
        let generation = timeline.generation();
        let structural_generation = timeline.structural_generation();
        let moved = clips[source_index];

        assert_eq!(
            timeline.move_clip_with_link(moved, Tick(delta), false),
            Err(TimelineError::Overlap { track, clip: moved }),
        );
        assert_eq!(timeline.snapshot(), before);
        assert_eq!(timeline.generation(), generation);
        assert_eq!(timeline.structural_generation(), structural_generation);
        timeline.check_invariants().unwrap();
    }
}

#[test]
fn distant_linked_collision_does_not_move_either_half() {
    let mut timeline = Timeline::new_default();
    let pair = timeline
        .insert_linked_av_pair(MediaId(1), Tick(60), Tick(10), Tick(0))
        .unwrap();
    let audio_track = timeline.clip(pair.audio).unwrap().track_id;
    timeline
        .insert_clip(audio_track, MediaId(2), Tick(0), Tick(10), Tick(0))
        .unwrap();
    let before = timeline.snapshot();
    let generation = timeline.generation();

    assert_eq!(
        timeline.move_clip_with_link(pair.video, Tick(-58), true),
        Err(TimelineError::Overlap {
            track: audio_track,
            clip: pair.audio,
        }),
    );
    assert_eq!(timeline.snapshot(), before);
    assert_eq!(timeline.generation(), generation);
    timeline.check_invariants().unwrap();
}

#[test]
fn moves_across_every_gap_preserve_order_lookups_and_collision_atomicity() {
    let mut base = Timeline::new_default();
    let track = base.tracks[0].id;
    let clips = [0, 10, 20, 30, 40, 50, 60, 70].map(|start| {
        base.insert_clip(track, MediaId(1), Tick(start), Tick(3), Tick(7))
            .unwrap()
    });
    let before = base.snapshot();
    for moved in clips {
        for target in 0..=83 {
            let mut timeline = base.clone();
            let original_start = timeline.clip(moved).unwrap().start.0;
            let overlaps = before.tracks[0].clips.iter().any(|other| {
                other.id != moved && target < other.end().0 && target + 3 > other.start.0
            });
            let result = timeline.move_clip_with_link(moved, Tick(target - original_start), false);
            if overlaps {
                assert_eq!(result, Err(TimelineError::Overlap { track, clip: moved }));
                assert_eq!(timeline.snapshot(), before);
                assert_eq!(timeline.generation(), base.generation());
            } else {
                result.unwrap();
                let mut expected = before.clone();
                expected.tracks[0]
                    .clips
                    .iter_mut()
                    .find(|clip| clip.id == moved)
                    .unwrap()
                    .start = Tick(target);
                expected.tracks[0].clips.sort_by_key(|clip| clip.start);
                assert_eq!(timeline.snapshot(), expected);
                for clip in &expected.tracks[0].clips {
                    assert_eq!(timeline.clip(clip.id), Some(clip));
                }
                let changed = u64::from(target != original_start);
                assert_eq!(timeline.generation(), base.generation() + changed);
                assert_eq!(
                    timeline.structural_generation(),
                    base.structural_generation() + changed
                );
            }
            timeline.check_invariants().unwrap();
        }
    }
}
