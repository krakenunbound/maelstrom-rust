# Phase 1 sustained four-source soak

`scripts/Run-Phase1SustainedSoak.ps1` is an opt-in, headless resource and
scheduler soak. It does not launch `Maelstrom.exe`, display the GUI, or modify
a project document. The wrapper starts the full absolute Cargo path; Cargo owns
the test child normally. It requires PowerShell 7 because timeout cleanup uses
the tracked Cargo process-tree API.

```powershell
.\scripts\Run-Phase1SustainedSoak.ps1
```

The default requested duration is 600 seconds. A bounded 15–30 second invocation
such as `-DurationSeconds 15` is plumbing-only; both reports mark it
`authoritative: false`. The wrapper marks a run authoritative only after a
passing requested duration of at least 600 seconds actually completes.

The wrapper holds an exclusive local fixture/artifact lock, then reuses
`Run-Phase1Multisource.ps1` to make the pinned,
dynamic five-second 1920x1080/30fps MPEG-4 fixtures. It then builds the release
`nle-app` test binary using a resolved absolute `cargo.exe` path, resolves its
exact path from Cargo metadata,
and starts the exact test through Cargo, hidden. It never directly starts the
resolved test executable. The wrapper identifies Cargo's descendant test process
and samples that PID with one-second polling sleeps. Its warm working-set baseline is compared with a deliberately generous
bound: aggregate decoded-frame-cache capacity plus 512 MiB allocator/codec
headroom. On timeout the runner stops only its tracked Cargo process tree.
Environment
variables are restored in `finally`.

## What the resource gate proves

Every completed four-source cycle checks current cache bytes and session counts
against their configured limits. The frame cache and session pool maintain exact
historical peaks on publication/acquisition, so the final peaks include activity
between cycle samples. A successful ten-minute run therefore qualifies the
configured decoded-frame cache and sticky-session limits for this local workload.
After app drop, the test waits for active sticky sessions to reach zero.

Source groups and live/retiring actors are sampled per cycle; this schema does
not retain an actor high-water mark or assert post-drop actor/cache release.
Those observations must not be described as continuous actor-memory accounting
or complete post-drop resource proof. Process working-set growth is sampled once
per second with a diagnostic cache-plus-headroom allowance, not a hard whole-app
RAM limit. The gate does not establish real-time playback, audible continuity,
visible UI latency, GPU composition, or another hardware profile.

The ignored test keeps one app with four video layers, Full selected and paused
quality, and explicit 1920x1080 output. It repeatedly requests all four media
at deterministic forward/backward mid-GOP positions in the five-second fixture
loop. Before the next cycle it polls for each current media/tick frame and
times out after five seconds. The report contains every scheduler submission
and frame-ready sample plus nearest-rank p50/p95/max summaries, exact runtime
counter deltas, four source exercise counts, the exact eight-tick forward/backward
pattern, maximum decoded-tick delta, decoder backend identities, and aggregate
cache/session metrics. It requires at least eight cycles and a decoded tick no
more than one 30fps frame (33,334 microseconds) after the requested tick.

The workload counter bounds are intentional: exactly four requests per completed
cycle; completed frames at least requests; presented frames exactly completed;
bounded rejected stale/non-converging monitor events and zero monitor errors; holds and late frames no greater than requests; native plus
fallback uploads exactly equal presented frames; and zero audio underrun,
audio-lock-failure, or audio-late-discard counters. Progressive monitor decode can
legitimately make completed frames exceed requests and can record bounded holds or
late frames, so those are not incorrectly forced to zero. The stale-event limit is
`max(4, ceil(requests / 1000))` (0.1%, with a four-event startup floor);
it counts deliberately rejected obsolete/non-converging events rather than displayed-frame
loss. Cache current/peak and
session active/peak/lane totals and source-group/live/retiring actor totals must
remain coherent with their owned caps, and
dropping the app must release all sessions.

Both reports are atomically written below ignored
`artifacts/phase1-sustained/`. The wrapper includes the exact Cargo/test executable
path/SHA-256, fixture paths/sizes/SHA-256, PID, memory samples/baseline/peak/growth/bound,
app-report path/SHA-256, and captured test stdout/stderr so a failed run retains evidence
when possible. The wrapper is authoritative only when the test passed, both
reports mark a requested duration of at least 600 seconds, and the finite actual
duration reached the requested duration.

If a final gate threshold fails, the app atomically writes the complete schema-1
report with `status: "failed"` before the ignored test returns failure. The wrapper
reads and hashes that report before handling the nonzero test exit, preserving the
same evidence path for failed and passing runs.

This is headless local Software/backend evidence for a repeated fixture-loop
scrub-resource workload. It is not evidence of realtime playback, audio,
visible UI responsiveness, GPU compositor performance, export parity, or
cross-hardware behavior. The broad Phase 1 exit gates remain unchecked.

## Recorded authoritative local evidence

The committed gate passed on 2026-08-29 for 600.031 seconds. It completed
15,195 four-source cycles and 60,780 requests with 37 us input-to-submit p95
(183 us maximum), 48 ms coarse frame-ready p95 (76 ms maximum), four rejected
stale/non-converging events under the 61-event limit, and zero monitor errors.
The cache ended at 207,360,000 bytes with a 215,654,400-byte peak upper bound
under its 1 GiB cap. The shared session pool peaked at seven of eight sessions and
released to zero after app drop. Working-set growth was 89,128,960 bytes under
the 1.5 GiB diagnostic bound. The observed decoder backend was Software.

Local ignored evidence remains at
`artifacts/phase1-sustained/phase1-sustained-wrapper.json`. The exact release
test executable SHA-256 was
`7d18a20f9fe4357b5f5e4986f7f129755db0bffe4c66153336e5dfbb7f3e90d8`; the
app-report SHA-256 was
`9dfe0003242cffdb47e90ff2f192e1469cb3db7ab2cacddd9eb5b7fe38ee1ef3`.

## Pre-source-actor shared-cache evidence

After consolidating the four decoder-local caches into one app-wide hard-capped
cache, the same gate passed again on 2026-08-29 for 600.032 seconds. It completed
14,899 four-source cycles and 59,596 requests with 38 us input-to-submit p95
(2,675 us maximum), 49 ms coarse frame-ready p95 (76 ms maximum), six rejected
stale/non-converging events under the 60-event limit, and zero monitor errors.
All 67,335 completed frames were presented. The physical cache ended at
207,360,000 bytes and reached an exact 215,654,400-byte peak under its 1 GiB
cap. The session pool again peaked at seven of eight sessions and released to
zero after app drop. Working-set growth was 85,987,328 bytes under the 1.5 GiB
bound. The observed decoder backend was Software. This historical run predates
source-owned actor/session deduplication.

The ignored wrapper is
`artifacts/phase1-sustained/phase1-sustained-shared-cache-wrapper.json`. The
exact release test executable SHA-256 was
`d74dbff796ad692b280de89c2c47d39dbef1d0c1ee8f013900c5c3e1a077897f`; the
app-report SHA-256 was
`d30a9d074dc4f73919c78aefdfe5416596dab3d7ccf95fa347a1aed50717d86d`.

## Post-source-actor authoritative evidence

After source-owned actor/session deduplication and the hard combined
live-plus-retiring actor reservation were added, the gate passed on 2026-08-29
for 600.020 seconds. It completed 16,494 four-source cycles and 65,976 requests
with 44 us input-to-submit p95 (416 us maximum), 45 ms coarse frame-ready p95
(64 ms maximum), seven rejected stale/non-converging events under the bounded
allowance, and zero monitor errors. All 67,557 completed frames were presented.
The physical cache ended at 207,360,000 bytes and reached an exact 215,654,400-byte
peak under its 1 GiB cap. The source coordinator held four groups and five live
actors (four foreground plus one shared speculative background) under its
eight-actor cap, with zero retiring actors at the final sample. The session pool
peaked at five of eight and released to zero after app drop. Working-set growth
was 40,943,616 bytes under the 1.5 GiB diagnostic bound. The observed decoder
backend was Software.

The ignored wrapper remains
`artifacts/phase1-sustained/phase1-sustained-wrapper.json`. The exact release test
executable SHA-256 was
`89106d9f98648cf3fe597b5280083c9f13058ccda6224eb2bd01b81089cdc00e`; the
app-report SHA-256 was
`6f4a16497556d145a0bd244e1ff46a8b9aa5fe6227cc7453714d4fdd1b177216`.

## Checkpoint resource qualification (2026-08-30)

The unchanged production source at `b1d5acec07e3f6c44b054ccce73aec228ae0dc74`
passed a fresh 600.042-second run after the shared-history and export fixes.
It exercised all four Full-1080p sources in each of 15,933 forward/backward
cycles, totaling 63,732 requests. Scheduler submission p50/p95/max was
41/54/304 us; coarse all-frames-ready timing was 36/47/80 ms. The maximum
decoded-tick deviation was 17,667 us within the 33,334 us bound.

The exact decoded-frame cache peak was 215,654,400 bytes, with 207,360,000
bytes retained at the final sample, below the configured 1,073,741,824-byte cap.
The session pool peaked at five of eight sessions (four foreground and one
background at the final sample). Active sessions reached zero after app drop.
The final sampled coordinator state was four source groups, five live actors,
and zero retiring actors; this is not an actor high-water or post-drop claim.

There were 66,925 completed/presented frames, zero current monitor errors, and
twelve rejected stale events within the 64-event allowance. Stale rejection
is not displayed-frame loss. The backend was Software. No native audio device
or visible GUI participated in this gate.

The wrapper retained 521 working-set samples. Warm baseline was 352,735,232
bytes, peak was 395,743,232 bytes, and growth was 43,008,000 bytes under the
diagnostic 1,610,612,736-byte cache-plus-headroom allowance. No task-owned
Cargo, compiler, test, Maelstrom, FFmpeg, or FFprobe process remained.

An independent report check recomputed nearest-rank distributions and working-set
peaks from raw samples, checked exact caps/source counts and post-drop sessions,
and matched the fixture, binary, and app-report hashes. This closes the local
ten-minute configured cache/session cap gate. UI-present, cross-hardware, native
audio, whole-app RAM, and broader soak qualifications remain separate.

Evidence in ignored `artifacts/phase1-sustained`:

- `phase1-sustained-b1d5ace-wrapper.json`: SHA-256
  `a0e7937482bf059cb15b5b6d2368565b0ca7e0e54812056d59c7df9bc018c1c7`
- `phase1-sustained-b1d5ace-app-report.json`: SHA-256
  `c3ebdab8a6059cc993b0b8d2f809beec51a8bb4b2bfbf0126dd102639a26d038`
- `phase1-sustained-b1d5ace-verification.json`: SHA-256
  `cd1f404a1da45d7621d459725e1cc0e4d4ba4e41c74d520c91db35bc308bb1a8`
- `phase1-sustained-b1d5ace-test.stdout.txt`,
  `phase1-sustained-b1d5ace-test.stderr.txt`, and `phase1-sustained-b1d5ace-run.log`

Exact release test executable SHA-256:
`71d29a73c1c21a4979f073c77ccded67bc1b41eaf5c1912b89bc9b29fda2453f`.
Earlier default-path artifacts were preserved in
`before-b1d5ace-20260830T151106/` before the runner reused its fixed output paths.

## Accurate threaded conversion resource qualification (2026-08-31)

The unchanged production source at `b3e9228939f4b3edf2c2d98a74cf1be0d5338ba5`
passed a fresh 600.047-second headless run. It completed 20,184 forward/backward
cycles across four Full-1920x1080 sources, with 80,736 requests. Default high-quality
conversion remained enabled. Scheduler submission p50/p95/max was 43/77/358 us;
coarse all-frames-ready timing was 32/42/58 ms. The maximum decoded-tick deviation
was 17,667 us within the unchanged 33,334 us bound. These are repeated-seek
measurements, not physical input latency or a continuous playback cadence.

The exact frame-cache peak remained 215,654,400 bytes, with 207,360,000 bytes
retained at the final sample, below the 1,073,741,824-byte configured cap. The
session pool peaked at five of eight and released to zero after App drop. The
final coordinator sample held four source groups, five live actors (four foreground
and one background), and zero retiring actors. This is not a post-drop actor or
actor high-water claim.

All 83,773 completed frames were logically presented through the fallback viewer
path. There were zero monitor errors and zero rejected stale events under the
81-event allowance. Hold/late counters were 10,915/10,918, within the workload's
request-count bounds; they are not proof of lossless real-time display. The observed
backend was Software. No visible editor or native audio device participated.

The wrapper retained 521 working-set samples: warm baseline 379,654,144 bytes,
peak 399,466,496 bytes, growth 19,812,352 bytes. Growth remained below the diagnostic
1,610,612,736-byte cache-plus-headroom allowance. This is a sampled test-process
working set, not a whole-app RAM hard cap. The exact tracked Cargo and test processes
exited, and no task-owned compiler/editor/media process remained.

The local adapter `run-threaded-scaler-soak.ps1` preserves the committed runner's
assertions, duration, process ownership, memory sampling, timeout, and cleanup.
It checks all 27 prior checkpoint evidence/input hashes and re-probes the existing
four MPEG-4 fixtures before skipping their regeneration. Only fixed root bindings
and distinct evidence filenames are substituted, each with an exact single-match
guard. Its first attempt failed before test launch because the in-memory runner had
an empty script root; the corrected explicit paths and initial failure log are
preserved. No assertion or limit was relaxed. Prior evidence and fixtures remain
unchanged; all 27 identities were checked again after the run.

Independent checks recomputed nearest-rank distributions from both 20,184-sample
arrays, checked cycle/source/counter/tick contracts and exact resource caps, derived
working-set summaries from raw samples, and matched fixture, test-binary, and
app-report hashes. This qualifies the local ten-minute cache/session resource gate
after accurate threaded conversion, not windowed UX, native audio, cross-hardware
playback, or lower-core CPU contention.

Ignored evidence under `artifacts/phase1-sustained/`:

- `threaded-scaler-wrapper.json`: SHA-256
  `40254F7E884FCBDDACED59CB028E5A6CC2A4C6ADF21D48855D1896F58F245E6C`
- `threaded-scaler-app-report.json`: SHA-256
  `F6693B3650D79B3E5B68D00A01966A8B7889D65EDE2976124213D33807D1BE08`
- `threaded-scaler-test.stdout.txt`, `threaded-scaler-test.stderr.txt`,
  `threaded-scaler-run.log`, `threaded-scaler-adapter-setup-failure.log`,
  `run-threaded-scaler-soak.ps1`, and `threaded-scaler-verification.json`.

The 22-entry post-run provenance snapshot includes both the committed runner and
local adapter, plus raw reports, logs, fixtures, source files, and binaries. All
entries were rehashed successfully. This is not a signed execution attestation.
Verification-manifest SHA-256:
`96EAB3F2E8C87ED39FDDA14BB4779A10C22C2DD3B3B8839937A246859FB18F51`.

Exact release test executable SHA-256:
`FBC83DDA56C407617AD6AAD438DDCA2A12A6B86D273683E36D8809FE96FFA484`.
The portable editor remains unchanged at SHA-256
`3A4D88D8672810349A2921C90371ED7E1A679CB0821A66BDA0953ADFF1F574C8`;
its GUI smoke status remains `not_run`. Four one/four-source Intel/NVIDIA cases
were prepared without launch in
`artifacts/phase1-multisource/windowed-6ab6b596-521d-43b1-8b7a-66a61d90f641/`.
Their configuration/package hashes verify, with status `prepared` and no GUI reports.
Windowed execution still requires explicit authorization through the exact batch launcher.

## Direct RGBA packing resource qualification (2026-08-31)

The unchanged production source at `e0f35d155c42af307581d216e6798127e4e8d43c`
passed a 600.0337152-second headless run after single-allocation RGBA packing.
It completed 21,072 forward/backward cycles across all four Full-1920x1080
sources, totaling 84,288 requests. The exact eight mid-GOP tick positions and
high-quality conversion remained unchanged. Scheduler p50/p95/max was
44/79/396 us; coarse matching-frame timing was 30/41/58 ms. Both distributions
were recomputed from their 21,072 raw samples. Maximum decoded-tick deviation
was 17,667 us, below the unchanged 33,334 us bound.

The frame cache peaked at 215,654,400 bytes and retained 207,360,000 bytes at
the final sample, below the 1,073,741,824-byte cap. Sessions peaked at five of
eight and fell to zero after App drop. Final coordinator state was four source
groups, five live actors (four foreground and one background), and no retiring
actors. This is not a post-drop actor or actor high-water claim.

All 88,064 completed frames were logically presented through the fallback viewer.
There were zero monitor errors and three rejected stale events within the
85-event allowance. Hold/late counters were 10,611/10,614, within the unchanged
request-count limits. These counters do not prove lossless visible playback.
The observed decoder backend was Software; no editor window, native GPU viewer,
or audio device participated. Zero audio counters are therefore not an audio test.

The wrapper retained 522 working-set samples through 600.200 seconds. Its first
sample after five seconds was at 5.934 seconds: 350,576,640 bytes. Peak working
set was 385,839,104 bytes, giving 35,262,464 bytes of growth under the
1,610,612,736-byte cache-plus-headroom diagnostic allowance. This is sampled
test-process memory, not an application RAM hard cap or leak-free certification.

The run-specific `run-rgba-packing-soak.ps1` adapter checks all 36 packing
checkpoint identities and re-probes the four existing five-second MPEG-4 fixtures.
It preserves the committed runner's assertions, duration, timeout, process
ownership, memory sampling, and cleanup. Exact single-match substitutions supply
fixed root paths, skip fixture regeneration, and use distinct output filenames.
No historical report or fixture was overwritten; all 36 identities were checked
again after completion. No acceptance rule was relaxed.

The adapter is local evidence routing, not a concurrency-safe standalone release
runner: its hash/non-overwrite preflight precedes the committed runner's fixture
mutex. This run was observed in isolation with clean tracked source at the stated
commit before launch. The runner's in-lock fixture provenance matches the prior
manifest; the source, test binary, runner, and adapter hashes were checked during
the run and after completion. Before reusing this adapter concurrently, move its
preflight under the same lock. The baseline commit in the earlier packing manifest
describes the pre-change baseline, not the source being qualified by this run.

The separate `verify-rgba-packing-soak.ps1` audit recomputes raw timing and memory
summaries, checks continuous four-source exercise, exact tick/counter/resource
contracts, reconciles fixture/report/test hashes, and confirms tracked process
exit. Independent review also checks the local qualification evidence. All four
observed Cargo/runtime/test PIDs exited; no task-owned compiler, test harness,
editor, FFmpeg, or FFprobe remained. No product code changed during qualification.

Ignored evidence under `artifacts/phase1-sustained/`:

- `rgba-packing-wrapper.json`: SHA-256
  `BDDC6E3D10A0AF3AB0CC8453C522A68C4D16D628ED9839865345E2FB46E75877`
- `rgba-packing-app-report.json`: SHA-256
  `7201F3020172B36B8660835BD80CFEE2AC8DEBAC909F6921CA051C4BE0312267`
- `rgba-packing-test.stdout.txt`, `rgba-packing-test.stderr.txt`,
  `rgba-packing-parent-audit.log`,
  `run-rgba-packing-soak.ps1`, `verify-rgba-packing-soak.ps1`, and
  `rgba-packing-verification.json` (post-run provenance snapshot, not an execution
  attestation).

Exact release test executable SHA-256:
`C979487D887AC730A9C951D71953DD724AF9D93D4ADB7938168E45D55691DC20`.
All 24 post-run snapshot entries were rehashed successfully. Verification-manifest
SHA-256: `768527FE2FB3D5366AAEF5E6F0BA0C5AD32A932F3CEFEC65B781B8DE977058AB`.
The editor package is unchanged at SHA-256
`4715E17526744CC803748660E8BD978FA8C2917E682435E88E15146A3006F49A`;
GUI smoke remains `not_run`. This qualifies local ten-minute cache/session
resources, not real-time display, physical input/scanout, native audio, lower-core
contention, or cross-hardware performance. Windowed testing still needs explicit
launch authorization and refreshed prepared-case identities for this package.
