# Performance reports

Maelstrom's existing surface-submission probe writes a schema-version 7 JSON report when
`MAELSTROM_SURFACE_SUBMISSION_REPORT` names an output file. Frame/submission and bounded viewer
timing samples measure a fixed 120-frame window; decoder-worker and audio timing aggregates are
cumulative at publication. Report serialization and disk IO remain on the existing one-shot,
off-thread worker.

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
- optional isolated viewer-compositor GPU execution timing at
  `gpu_stage_timings.composite_pass_gpu`. `timestamp_query_supported` states whether the selected
  adapter enabled wgpu timestamp queries; unsupported adapters serialize the pass timing as JSON
  `null` and continue normally. Supported adapters bracket only the changed-composition render pass,
  convert the two device timestamps with wgpu's timestamp period, and retain a fixed 120-sample
  p95/maximum window. One asynchronous readback may be in flight; later samples are skipped rather
  than blocking or reusing mapped memory.
- GPU submission-completion timing at
  `gpu_stage_timings.submission_to_completion_elapsed`. This is CPU monotonic elapsed time from
  immediately before `queue.submit` until wgpu reports that all GPU work through that submission
  has completed. It can include earlier queue backlog, driver scheduling, and the measured
  submission. It is not isolated GPU-pass execution time and excludes the later presentation
  handoff, DWM composition, and physical scanout. Callback dispatch and the non-blocking polling
  cadence can extend the observed elapsed time, so this is a completion observation rather than a
  precise hardware timestamp. The opt-in editor probe keeps at most one
  callback in flight, services it only with non-blocking device polling, skips new samples while
  one is pending, and retains a fixed 120-sample p95/maximum window.
- native audio output-callback CPU timing at `audio_stage_timings.output_callback_cpu`, with sample
  count, total, mean, and maximum milliseconds (no p95), retaining the whole callback boundary.
- native audio mix/render CPU timing at `audio_stage_timings.mix_render_cpu`, with the same fields.
  This starts after a successful mixer-state lock and includes lane mix/fades/effects, output sample
  conversion, meter accumulation/store, device-clock advancement, and underrun bookkeeping. A lock-
  failure silence fallback produces no mix/render sample; an acquired paused callback counts.
  Both audio fields use monotonic elapsed time around CPU-side work, so scheduler preemption may be
  included; they are not per-thread CPU-accounting measurements.
- a nested `runtime_diagnostics` snapshot containing cumulative
  `monitor_requests`, `monitor_completed_frames`, `monitor_presented_frames`,
  `monitor_dropped_frames`, `monitor_hold_events`, `monitor_late_frames`, `monitor_errors`,
  `native_viewer_uploads`, `fallback_viewer_uploads`, `audio_underrun_frames`,
  `audio_callback_lock_failures`, and `audio_late_discarded_frames`. These counters are cumulative
  since process/session start and are not scoped to the fixed 120-frame timing window.

This is CPU, submission-cadence, isolated compositor-pass GPU, and whole-submission completion
evidence. It does not claim DWM composition, physical scanout, or end-to-end display latency. The
other timing stages likewise end at their named CPU boundaries; the audio callback
timing excludes device/DAC work and GPU/display completion. A standalone
cadence probe may report an empty decoder list and `encoder_backend: "not_observed"` when that run
did not exercise media. The full package media smoke defers the report until a decoder has produced
a frame, all applicable decoder timing stages have completed samples, successful native viewer
upload and changed-composition samples exist, at least one measured GPU submission has completed,
an isolated compositor GPU sample exists when timestamp queries are supported,
both native audio timing boundaries have completed samples, and an encoder process has actually
started. Its backend and timing fields are therefore
evidence rather than hardware guesses. Project files never contain this session-only metadata.

Facts unavailable through a supported platform API are serialized as JSON `null`; zero and
`"Unknown"` are not used as hidden unavailable-value sentinels.

Windows packaging performs structural and exercised-path validation: it checks the packaged report
shape and confirms the full report includes real CPU/RAM, renderer, media backend, preview, cache,
display, and exercised runtime-counter data when available before accepting the build. Existing
CPU/cadence package limits remain in force; counter-rate and long-session health thresholds remain
in the dedicated playback-soak and sustained-soak runners. The retained report is
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

The app atomically writes schema-version 4 `playback-soak-app-report.json` under the ignored
`artifacts/phase0-playback-soak` directory. It contains requested and actual wall duration,
completed loop count, observed decoder backends, selected/resolved preview quality, configured
monitor-cache cap, `monitor_resources`, and deltas for monitor
requests/completions/presents/drops/holds/lates/errors, native/fallback viewer uploads, audio
underruns, callback lock failures, and late audio discards. `monitor_resources` reports the one
app-wide hard-capped decoded-frame cache; capacity, current bytes, and
`peak_frame_cache_bytes_upper_bound` therefore describe one physical allocation budget rather than
a sum of decoder-local caches. Session fields come from one shared hard permit pool:
`active_sticky_sessions`, exact `peak_sticky_sessions`, and `session_cap`, plus exact
foreground/background active counts and caps. Source ownership is reported separately through live
source-group/group-cap and live/retiring lane-actor diagnostics.

The runner validates actual duration, a `Full` selected/resolved preview, backend evidence, at least
20 native uploads per second, a healthy A/V transport at completion with no observed audio fault or
early stop, positive cache/session peak exercise, cache current and peak upper bound no greater than
aggregate capacity, exact active and peak sessions no greater than the shared cap, coherent
foreground/background totals, bounded source groups, combined live-plus-retiring lane actors no
greater than the lane-actor cap, zero monitor errors/fallback uploads, no more than 2% late monitor requests, and zero audio
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
frame-cache sample ended at 264,456,192 bytes and its then-current summed decoder-local peak upper bound was
268,015,616 bytes against the 1,073,741,824-byte aggregate cap. Active/peak sticky sessions were
1/1 against the 16-session app cap. Peak GUI working-set growth was 237,711,360 bytes. The ignored
local schema-version 3 evidence remains in
`artifacts/phase0-playback-soak/playback-soak-report.json`; it predates the shared physical cache
and source-actor diagnostics.
