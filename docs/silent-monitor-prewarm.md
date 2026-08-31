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

Restricted-CPU reruns remain pending. Do not treat the production fix as sustained,
native-audio, windowed, or cross-machine qualification until the corresponding
results are recorded here. The local `run-silent-prewarm-soak.ps1` adapter preserves
the committed gate's assertions and fixtures, requires an exact clean source
commit before/after, and samples affinity from the actual Cargo test process.

The existing package remains unchanged at executable SHA-256
`03E01F2EB32BFA3B301C161C638829257C2DDD1B0A78C604E070F25D365D6DFA`.
It does not yet contain this fix. No editor was launched for this investigation.
