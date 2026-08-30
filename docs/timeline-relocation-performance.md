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

**At this relocation-only checkpoint, the complete 2 ms gate remained open.** Six of ten trials passed both limits.
Three failed pointer-press capture (2.2894, 2.0676, 2.0763 ms), and one failed
edit/release (2.4516 ms). No failed trial was discarded or threshold changed.
The pointer-press path still cloned a full before-state snapshot; reducing this
cost and the remaining release tail required further work. These short local
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

## Shared-record checkpoint

The next change keeps the complete before-state but makes each `Clip` a shared
immutable `Arc<ClipData>` record. `snapshot()` copies contiguous handle arrays
instead of every field and nested effect vector. Mutable access uses
`Arc::make_mut`, so editing a clip detaches its data from history, export, and
other snapshots. History skips records sharing the same immutable allocation,
then uses full value equality for independently allocated records. Public clip
equality remains value-based, including the existing non-reflexive NaN behavior.

Canonical snapshot restore retains sharing. It computes normalized fields
first and detaches only changed records. Nonempty video-effect vectors are
still cloned for normalization checks; restore is not a constant-time operation.
The tradeoff is one heap allocation and reference-count overhead per clip,
plus pointer indirection during uncached scans. Retained draw caches keep their
compact columns. Flat project JSON and legacy defaults are unchanged; Rust
callers constructing clips must use `Clip::new(ClipData { ... })` instead of a
`Clip` struct literal. All workspace constructors were updated.

Ten independent release invocations of the original editor test passed both
unchanged 2 ms limits and its undo/redo assertions. No trials were discarded.
Nearest-rank measurements are in milliseconds:

| Stage | p50 | p95 / max |
|---|---:|---:|
| Pointer-press checkpoint | 0.2544 | 0.3481 |
| Move only | 0.4144 | 0.6038 |
| History record | 0.2554 | 0.5223 |
| Edit plus release | 0.6663 | 0.9921 |

**The local release history gate now passes.** This is headless CPU evidence,
not UI-present, packaged, cross-hardware, or ten-minute soak qualification.
The broader foundation verification remains open for those qualifications.

Existing dense drawing evidence also passed: wide/detail/playhead p95 was
0.4656/0.3434/0.3489 ms, with 737 wide and 277 detail primitives. Cache rebuild
plus zoom-out banding took 0.8655 ms and emitted 1,000 records. The separate
inverse-history test measured 0.2894 ms snapshot capture and 0.8163 ms recording
(including its after-state snapshot). These short runs check current budgets;
they are not an allocation, export-throughput, or effect-heavy restore benchmark.

Eight integration regressions prove shared canonical restore, scalar and nested
keyframe isolation, normalization without altering the input snapshot, exact
flat JSON/legacy restoration, normal value equality, independent equal records
preserving redo, and a 50,000-clip move plus late probe update detaching exactly
the two changed records. Undo/redo restores complete snapshots. The reverse-scrub
decoder test now waits for zero live/retiring lanes and sticky sessions before
deleting its media fixture; ten focused release repetitions passed.

Retained evidence in `artifacts/phase1-multisource`:

- `shared-clip-history-{1..10}.log` and `shared-clip-summary.json`
- `shared-clip-ui-cpu.log`, `shared-clip-cache.log`, `shared-clip-isolation.log`
- `decoder-teardown-{1..10}.log`

Summary SHA-256:
`56A30822B5A971AA31C6CCFB7F3757061555B04F6E65AC0CA7BC4EF64F141147`.

Final serial release workspace verification passed 715 tests, and strict
all-target workspace Clippy passed (`shared-clip-workspace-serial.log` and
`shared-clip-clippy.log`). Independent review found no blocking ownership or
history issues. No editor was launched and no task processes remain running.

The parallel workspace rerun was **not clean**. Its two-second equal-power audio
crossfade export remained CPU active for several minutes, grew its output to
about 12.8 MB, and emitted non-monotonic AAC DTS values near the negative i64
limit. The exact stalled FFmpeg child was stopped; the suite reported 47 export
tests passing and one failing after 173.34 seconds. That same export test had
passed in the preceding run and passed again in the serial rerun. Root cause
is unresolved; a serial pass does not erase the failure. The retained three
inputs, filter graph, and process command are in `shared-clip-export-stall/`;
the failure log is `shared-clip-workspace-final.log`. `run_child_with_encoder`
already drains stdout and stderr on separate threads, so the observed active
encoding and corrupt timestamps do not by themselves establish a pipe deadlock.
Export timing/termination investigation remains the next foundation task.

The earlier `shared-clip-workspace.log` also retains a failed newly authored
NaN test expectation, corrected to match the original value-equality contract
before the final eight-test isolation suite and serial workspace verification.
