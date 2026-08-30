# Performance reports

Maelstrom's existing surface-submission probe writes a schema-version 3 JSON report when
`MAELSTROM_SURFACE_SUBMISSION_REPORT` names an output file. It measures a fixed 120-frame window;
report serialization and disk IO remain on the existing one-shot worker.

The report records:

- CPU frame-time p95, surface-submission interval p95, and average submission rate;
- the renderer adapter, vendor/device IDs, wgpu backend, and driver strings reported by wgpu;
- every decoder backend that produced an observed monitor frame and the encoder process most
  recently started by the export fallback chain;
- CPU identity, logical processor count, and total physical RAM;
- selected and resolved preview quality, requested decode dimensions, monitor-cache cap, and
  current display refresh rate.
- aggregate decoder-worker CPU timing stages across all bounded monitor lanes: cache lookup,
  demux packet retrieval, decoder send/receive/flush calls, hardware-to-CPU transfer, scaler
  work, RGBA copy plus letterbox, and whole worker request. Each stage has sample count, total,
  mean, and maximum milliseconds. Software decode legitimately reports zero transfer samples.
- viewer-stage CPU/API submission timing: `viewer_stage_timings.upload_cpu` times only successful
  `HubRenderer::upload_viewer_layer_rgba` calls (including a resize allocation when needed), and
  `viewer_stage_timings.compositor_encode_cpu` times only callbacks that actually encode changed
  composition work. Each has a fixed 120-sample p95/max snapshot. `surface_present_call_cpu_p95_ms`
  brackets only the `frame.present()` API call; the existing `cpu_p95_ms` ends before that call.
- native audio output-callback CPU timing at `audio_stage_timings.output_callback_cpu`, with sample
  count, total, mean, and maximum milliseconds (no p95). It covers callback CPU through lock retry,
  mix, effects, conversion, and meter work, or the silence fallback when the callback cannot mix.

This is CPU and submission-cadence evidence. It does not claim scanout, GPU-completion, or end-to-end
display latency. The timing stages likewise end at their named CPU boundaries; the audio callback
timing excludes device/DAC work and GPU/display completion, and the report does not claim GPU
upload/compositing completion, GPU execution, or presentation/scanout. A standalone
cadence probe may report an empty decoder list and `encoder_backend: "not_observed"` when that run
did not exercise media. The full package media smoke defers the report until a decoder has produced
a frame, all applicable decoder timing stages have completed samples, successful native viewer
upload and changed-composition samples exist, native audio output callbacks have completed samples,
and an encoder process has actually started. Its backend and timing fields are therefore evidence
rather than hardware guesses. Project files never contain this session-only metadata.

Facts unavailable through a supported platform API are serialized as JSON `null`; zero and
`"Unknown"` are not used as hidden unavailable-value sentinels.

Windows packaging validates that the full report includes real CPU/RAM, renderer, media backend,
preview, cache, and display data when available before accepting the build. The retained report is
`dist/last-surface-submission-smoke.json`.

## Opt-in packaged playback soak

`scripts/Run-PlaybackSoak.ps1` exercises the real editor layout-backed Media Pool drop, then waits
until the normal audio-owned A/V transport has actually started before measuring wall time. At the
end of each real timeline pass it rewinds to zero and starts the ordinary seek/decode/audio path
again. It does not lower preview quality, alter the audio callback, add per-frame logging, or run
during a normal startup.

Run the full Phase 0 gate only against the explicit package path:

```powershell
& 'H:\Maelstrom Rust\scripts\Run-PlaybackSoak.ps1' `
  -ExecutablePath 'H:\Maelstrom Rust\dist\Maelstrom-Windows-x64\Maelstrom.exe' `
  -DurationSeconds 600
```

For a quick plumbing check, pass an explicit short duration such as `-DurationSeconds 15`; that is
not evidence for the ten-minute gate. The runner requires sibling packaged `ffmpeg.exe` and
`ffprobe.exe`, builds a deterministic 60-second A/V clip, and launches only the supplied absolute
executable path. During the opt-in run the editor window remains visible and always-on-top to avoid
Windows occluded-surface throttling; it returns to normal level when the application report is
written.

The app atomically writes schema-version 3 `playback-soak-app-report.json` under the ignored
`artifacts/phase0-playback-soak` directory. It contains requested and actual wall duration,
completed loop count, observed decoder backends, selected/resolved preview quality, configured
monitor-cache cap, `monitor_resources`, and deltas for monitor
requests/completions/presents/drops/holds/lates/errors, native/fallback viewer uploads, audio
underruns, callback lock failures, and late audio discards. `monitor_resources` sums cache
capacity/current bytes across the app's monitor decoders. Cache peaks remain the explicit summed
upper bound `peak_frame_cache_bytes_upper_bound`. Session fields come from one shared hard permit
pool instead: `active_sticky_sessions`, exact `peak_sticky_sessions`, and `session_cap`, plus exact
foreground/background active counts and caps.

The runner validates actual duration, a `Full` selected/resolved preview, backend evidence, at least
20 native uploads per second, a healthy A/V transport at completion with no observed audio fault or
early stop, positive cache/session peak exercise, cache current and peak upper bound no greater than
aggregate capacity, exact active and peak sessions no greater than the shared cap, coherent
foreground/background totals, zero monitor errors/fallback uploads, no more than 2% late monitor
requests, and zero audio
underruns/callback lock failures/late discards. It then atomically writes
`playback-soak-report.json` beside the app report with the exact executable path and SHA-256 plus
coarse WorkingSet64 evidence.

WorkingSet64 is sampled about once per second from only the exact GUI process launched by the
runner. Its warm baseline is the third sample, and the final report records peak/final/growth plus
a deliberately generous 1.5 GiB peak-growth bound. This is not total system memory, GPU memory,
or child-process memory, so it detects only coarse sustained GUI working-set growth rather than a
complete leak diagnosis. The runner always restores its environment and terminates only its tracked
PID tree in `finally`.

The latest 2026-08-28 Windows checkpoint passed against packaged executable SHA-256
`C41F3F1552ADBA6C30A4CA5F93580A93727AA1C51196A9877060198651B16CF5`: 600.006 seconds,
10 timeline loops, `Full` selected/resolved quality, 18,015 native viewer uploads, zero monitor
errors/fallback uploads, and zero audio underruns/callback lock failures/late discards. The decoded
frame-cache sample ended at 264,456,192 bytes and its summed decoder-local peak upper bound was
268,015,616 bytes against the 1,073,741,824-byte aggregate cap. Active/peak sticky sessions were
1/1 against the 16-session app cap. Peak GUI working-set growth was 237,711,360 bytes. The ignored
local evidence remains in `artifacts/phase0-playback-soak/playback-soak-report.json`.
