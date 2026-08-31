# Silent monitor prewarming

## Observed failure

The 2026-08-31 four-CPU-affinity qualification at source
`c9b761121850cfdd9e5ffc36af8717be8ee66c37` **failed** the unchanged ten-minute,
four-source Full-1080p resource gate. It ran for 600.035 seconds with 17,100 cycles
and 68,400 requests. Scheduler submission p95 was 991 us (limit 1,000 us), but
289 stale/non-converging frame events exceeded the 69-event allowance. There were
no decode errors; cache/session/working-set bounds passed and sessions reached
zero after App drop. This is headless Software evidence, not visible playback
or audio continuity proof. Four-CPU affinity is not an actual lower-core machine.

A separate 30-second test-only trace found 17 rejected events. All were on the
topmost layer, carried the previous request ID, and repeated the already displayed
source timestamp. That trace attributes those 17 events, not every event in the
uninstrumented ten-minute run. The diagnostic also failed its scheduler budget
(1,060 us p95); neither failure is erased by later results.

The original and traced test executables are separately archived in ignored
`artifacts/phase1-sustained/`. The trace was removed before the production fix.
`cpu-budget-4-failure-verification.json` independently checks the raw percentile
samples, event allowance, resource/memory bounds, 524 observed affinity samples,
the archived original executable, and all 17 diagnostic trace lines. Its 14
referenced files were rehashed successfully before reviewing the fix. This is a
local post-run hash snapshot, not a signed execution attestation.

## Cause and correction

Paused prewarming submitted visible foreground work plus speculative copies on
background workers. Those copies decoded and published final results into the
same presentation slot. A late copy could republish an equal request ID after
the foreground event had already been consumed. Its old, already displayed
frame then failed the application's legitimate convergence check.

The fix marks delivery intent on the private worker command. Speculative
prewarming still decodes, retains sessions, fills the bounded frame cache, and
records worker/backend diagnostics, but publishes neither progress nor terminal
frame/error events. Both standalone workers and shared source actors obey the
same rule; actor-acquisition errors also respect delivery intent. Foreground and
visible reverse-scrub requests retain their notifications and error delivery.

Public `DecodeRequest`, source pixels, resolution, scaling, cache/session caps,
and performance thresholds are unchanged. There is no generic frame deduplication
that could conflate different pixels or backend/fallback provenance.

Delivery intent remains invariant within a publicly routed decode generation:
speculative requests are non-scrubbing work on nonzero lanes, while visible work
on those lanes is scrubbing. `same_decode_generation` includes `is_scrubbing`.
Future routing changes must preserve that invariant or carry delivery intent
through retargeting; a private same-generation role switch is not supported.

## Verification status

Five added regression tests cover delayed speculative completion after foreground
drain; public prewarm fan-out in both standalone/coordinated workers; cache,
stage, backend and retained-session evidence; silent background decode errors;
visible reverse errors; and a barrier-controlled prewarm-to-newest-reverse
handoff with exact request/epoch/media assertions. Four prewarm-filtered tests
passed ten repeated release runs. The full release workspace passes 770 tests
(24 opt-in tests ignored), strict release/all-target Clippy, and formatting.
Independent review found no public-path blocker. Actor-spawn failure delivery is
statically inspected, not a forced runtime regression case.

A repeated full-suite run exposed an existing Windows test-cleanup race: the
shared-source reuse test deleted its fixture after the session permit returned
but before the actor finished closing FFmpeg's file handle (Windows error 32).
Its cleanup now additionally waits for the reaper's joined-actor count to reach
zero. The playback implementation and performance gate are unchanged by this
test-only correction. The failed run is retained in
`artifacts/phase1-sustained/silent-prewarm-workspace-verified.log`.

The local `run-silent-prewarm-soak.ps1` adapter preserves
the committed gate's assertions and fixtures, requires an exact clean source
commit before/after, and samples affinity from the actual Cargo test process.

### Restricted-CPU measurements

All runs below used clean source `b28cb4ebb1a98367fb9a287018c4c5e86ec12d51`,
the same four Full-1080p fixtures and unchanged 1,000-us submission limit.

| Allowed CPUs / duration | Result | Cycles / requests | Submit p95 | Frame-ready p95 | Drops / errors |
|---|---|---|---|---|---|
| 4 / 30.029 s | **Failed** | 687 / 2,748 | 1,042 us | 61 ms | 0 / 0 |
| 4 / 600.053 s | Passed | 18,132 / 72,528 | 924 us | 50 ms | 0 / 0 |
| 8 / 600.028 s | Passed | 21,655 / 86,620 | 98 us | 41 ms | 0 / 0 |

Requested, completed, presented, and uploaded frame counts match exactly in all
three runs. The long runs retain five sessions below the eight-session cap,
bounded cache and working-set growth, and zero sessions after App drop. Actual
affinity was observed 532 times at mask `F` and 545 times at `FF`; scaler probes
confirmed one and two scaling threads respectively. Independent review and the
v2 arithmetic/provenance audits pass, including all four source-file identities.
The short run's failure remains evidence of four-CPU timing variability; the
long-run pass does not erase it or prove reliable windowed/cross-machine latency.

The two long-run verification manifests under `artifacts/phase1-sustained/` are
`silent-prewarm-{4,8}-600s-1-verification-v2.json`. The corresponding 30-second
failure has its own report and verification file. The shared test executable
SHA-256 is `5899ABC2BF08DE40952776955F8DD9E25F60B462CB0F19F8114763FC21E687C5`,
archived in `silent-prewarm-b28-test-binary.zip` before changing the test harness.
These are post-run local snapshots, not signed attestations.

### Native-audio test follow-up

The subsequent four-CPU 30-second audio attempt **failed**, and the queued
eight-CPU audio test was not run. Audio itself had zero underruns, callback-lock
failures, or late discards; clock drift was 2,080 us and maximum observed clock
stall was 37 ms. The delayed video source recovered after 751 ms, while other
sources presented 45 frames and the audio clock advanced 750,000 ticks.

Video submission p95 was 11,418 us, and per-source presentations
`[97, 97, 97, 82]` were below the 120 minimum. Investigation found a harness defect:
the video fixture clips end at five seconds while the audio timeline lasts forty
seconds. Once the advancing playhead leaves those clips, preview requests contain
no video sources; changing the request's source timestamps does not recreate
them. Only 388 of the possible four-layer requests were sent across 586 timed
submission calls. Thus this is neither a passing audio/video gate nor evidence
of a full-duration four-video workload. Its measured latency failure remains real.

The report, stdout/stderr, and 28 live mask-`F` observations are retained under
`artifacts/phase1-multisource/silent-prewarm-4-live-audio*`, including the failure
verification manifest. Its audit recomputes the raw samples and verifies the
archived executable, pinned fixtures/runtime, and observed test-process affinity.
The test-only correction now extends video timeline coverage through measurement
plus warmup and asserts four exact sources on every timed submission. It preserves
all current thresholds, Full resolution, the continuous audio clock, and bounded
source timestamp wrapping. Two new regression tests and all 772 release workspace
tests pass (24 opt-in tests ignored), together with strict Clippy and formatting.
The corrected uninstrumented four-CPU run retained all sources but failed at
16,148 us submission p95 with one callback-lock failure and 480 underrun frames.
Two temporary test-only traces localize the dominant submission delay to the
source actor's wake notification, without proving its internal cause. The probes
were archived and removed; a one-second timeline boundary margin remains in the
test. Eight-CPU audio and a passing corrected qualification remain open. See
`docs/restricted-live-audio-submission.md` for evidence and next steps, and
`docs/phase1-live-audio.md` for historical coverage limits.

The existing package remains unchanged at executable SHA-256
`03E01F2EB32BFA3B301C161C638829257C2DDD1B0A78C604E070F25D365D6DFA`.
It does not yet contain this fix. No editor was launched for this investigation.
