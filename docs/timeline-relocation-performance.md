# Timeline relocation and history performance

The 2026-08-30 investigation preserved the existing ignored release test
`editor::tests::fifty_thousand_clip_editor_history_events_stay_under_two_ms`,
its 50,000-clip workload, both 2 ms thresholds, and undo/redo assertions. It
added separate move and history-record timings inside the original total timer.

Run from the workspace using the pinned Cargo executable and runtime runner:

```powershell
& 'C:\Users\The Kraken\.cargo\bin\cargo.exe' test -p nle-ui-core --release --lib editor::tests::fifty_thousand_clip_editor_history_events_stay_under_two_ms -- --ignored --exact --nocapture
```

Three pre-fix trials failed at 2.7406–2.8432 ms for edit/release. Move-only cost
was 1.6935–1.7177 ms; history recording was 1.0468–1.1359 ms. A relocated clip
caused a stable sort of the whole track and rebuilding of the entire timeline's
clip-location map, including unrelated tracks.

The edit kernel now binary-searches the new position and rotates only the
crossed range when one clip changes on a track. Linked A/V applies the same
operation independently to each affected track. Existing lookup entries are
updated in place without changing membership or growing the map. Multiple
changes on the same track retain the stable-sort fallback, refreshing only
that track's lookup entries. Snapshot formats, inverse history, transition
pruning, and edit generations are unchanged.

The investigation also reproduced an existing correctness defect: validating
only a clip's original neighbors allowed a jump onto a distant occupied clip.
Validation now checks the nearest unchanged neighbors at the destination plus
the final intervals of other clips participating in the edit. All checks occur
before mutation, including linked A/V failures. A 672-case reference oracle
covers gaps, occupied destinations, exact boundaries, left/right moves,
unchanged moves, sorted order, and generations. Additional tests check linked
atomicity, storage capacity, direct location-index correctness (without public
lookup's scan fallback), multiple-change sorting, and undo/redo snapshots.

Ten post-fix trials produced these nearest-rank measurements in milliseconds:

| Stage | p50 | p95 / max |
|---|---:|---:|
| Pointer-press checkpoint | 1.6747 | 2.2894 |
| Move only | 0.5434 | 1.2932 |
| History record | 1.0539 | 1.5804 |
| Edit plus release | 1.6175 | 2.4516 |

**The complete 2 ms gate remains open.** Six of ten trials passed both limits.
Three failed pointer-press capture (2.2894, 2.0676, 2.0763 ms), and one failed
edit/release (2.4516 ms). No failed trial was discarded or threshold changed.
The pointer-press path still clones a full before-state snapshot; reducing this
cost and the remaining release tail requires further work. These short local
trials do not establish live UI or cross-hardware latency.

Retained evidence is in the ignored `artifacts/phase1-multisource` directory:
`history-profile-baseline-{1,2,3}.log`, `history-destination-baseline.log`,
`history-profile-final-{1..10}.log`, and `history-profile-summary.json`.
The summary SHA-256 is
`91c28b2979357d3566c78b25ec564633708c69025992a12821c6e58cf906859e`.

Serial release workspace verification passed 707 tests; strict all-target
workspace Clippy passed. A preceding parallel workspace run hit an existing
Windows file-sharing error during cleanup in
`nle-decode::tests::speculative_release_preserves_visible_reverse_scrub_lane`:
the test deletes its fixture immediately after asynchronous decoder release.
That decoder source was unchanged and passed in the serial run; deterministic
teardown remains follow-up work. Both logs are retained as
`history-workspace-release.log` and `history-workspace-serial.log`, alongside
`history-workspace-clippy.log`. The editor was not launched.
