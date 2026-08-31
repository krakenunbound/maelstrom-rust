# Phase 1 live-audio continuity gate

## Coverage correction (2026-08-31)

Historical runs used five-second video timeline clips even when the audio test
ran longer. After the audio-driven playhead left those clips, submissions could
contain no video sources. Those reports retain their observed audio counters and
early slow-source isolation evidence, but do **not** prove continuous four-video
load beyond five seconds (including transport warmup). The corrected four-CPU
30-second run retained all four sources but failed submission latency and one
audio buffer; see `docs/restricted-live-audio-submission.md`.

The corrected test extends all four video clips through the requested duration
plus ten seconds of warmup allowance and one second of boundary slack, while
wrapping decoder timestamps inside
the original five-second media. Every measured submission now asserts four exact
source/layer identities and Full resolution. Two ordinary regressions cover the
long timeline and unchanged default five-second fixture. The release workspace
passes 772 tests (24 opt-in tests ignored), strict Clippy, and formatting. This
test-only correction changes no product code or performance threshold.

## Running the gate

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
teardown. It also holds the exact next request for the topmost real 1080p source
at the decoder worker boundary for 750 ms. During that hold, the gate requires
other sources to keep presenting and the native audio clock to advance by at
least 500,000 µs; after release it requires the delayed source to present again.
The delay hook is compiled only for tests and is absent from normal production
builds. Startup callbacks are excluded only after the native callback, mix,
source clock, and audible meter have all been observed. The schema-2 JSON report
is local-only and ignored.

The 2026-08-30 local Software-backend run recorded a 750 ms topmost-source hold,
45 ready-source presentations, and 750,000 µs of audio-clock
advance during the hold, then recorded 82 delayed-source presentations after
release. It completed 373 monitor presentations with zero monitor errors, audio
underruns, callback lock failures, or late discards; clock drift was 1,688 µs,
the maximum clock-progress interval was 22 ms, and no decoder session survived
teardown.

This is a bounded local default-device proof. It closes the local real-media
slow-source/playback-clock isolation gate, but it is not a GUI or cross-hardware
claim. The broader four-source UI-present/cross-hardware exit gate, VFR
cross-backend qualification, and later proxy extensions remain separate.
