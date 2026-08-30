# Phase 1 live-audio continuity gate

`scripts/Run-Phase1LiveAudio.ps1` is an opt-in native runtime gate. It uses
the four established Full-1080p video-only sources and creates a deterministic
60-second, stereo AAC tone at `artifacts/phase1-multisource/live-audio-60s.m4a`.
It first runs the existing four-source fixture gate, so the video inputs are
fresh and independently validated rather than assumed to exist.

```powershell
.\scripts\Run-Phase1LiveAudio.ps1
.\scripts\Run-Phase1LiveAudio.ps1 -DurationSeconds 10
```

The duration is bounded to 2–30 seconds and defaults to 5. The test runs only
through the pinned Cargo command and project-local runtime runner; it does not
launch the GUI or a raw target executable. The test opens the real default
output device, so the generated 440 Hz tone is audible while the four video
sources are exercised. Run it in a quiet environment and set the system output
level appropriately.

The runner validates schema/status, nonzero meter and callback/mix/source-tick
advancement, sustained nonzero measured-interval meter activity including the
final sample, wall-clock/device-clock drift and maximum stall (250 ms), a sustained
minimum request/presentation rate for every video source, zero monitor errors,
input submission p95 (1,000 µs), zero post-warmup audio underruns/lock
failures/late discards, fixture path/size provenance, and zero sessions after
teardown. Startup callbacks are excluded only after the native callback, mix,
source clock, and audible meter have all been observed. The JSON report is
local-only and ignored.

This is a bounded local default-device proof. It is not a GUI proof, a
cross-hardware claim, or the authoritative Phase 1 exit gate; broader VFR,
cross-backend, UI-present, proxy, and cross-hardware coverage remains separate.
