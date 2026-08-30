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

## Current-source resource qualification (2026-08-30)

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
