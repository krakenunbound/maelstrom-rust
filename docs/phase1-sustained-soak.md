# Phase 1 sustained four-source soak

`scripts/Run-Phase1SustainedSoak.ps1` is an opt-in, headless resource and
scheduler soak. It does not launch `Maelstrom.exe`, display the GUI, or modify
a project document. It requires PowerShell 7 because timeout cleanup uses the
tracked test process-tree API.

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
and launches only that test binary hidden. The tracked PID is sampled once per
second. Its warm working-set baseline is compared with a deliberately generous
bound: aggregate decoded-frame-cache capacity plus 512 MiB allocator/codec
headroom. On timeout the runner stops only that tracked test process tree.
Environment
variables are restored in `finally`.

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
session active/peak/lane totals must remain coherent with their owned caps, and
dropping the app must release all sessions.

Both reports are atomically written below ignored
`artifacts/phase1-sustained/`. The wrapper includes the exact test executable
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
