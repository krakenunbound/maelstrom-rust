# Performance reports

Maelstrom's existing surface-submission probe writes a schema-version 9 JSON report when
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
  `named_decoder_reopen` separately measures synchronous named hardware-decoder reopen attempts;
  zero samples are valid for software, native Windows hardware decode, CUVID, and QSV sessions
  that never reverse-seek. It includes failed reopen attempts and does not measure GPU completion
  or end-to-end scrub latency.
  Since the single-allocation packing checkpoint, RGBA packing includes the final shared-buffer
  allocation. Earlier reports excluded the last Vec-to-Arc allocation/copy; do not compare those
  historical stage spans directly. See `monitor-rgba-packing.md` for the paired measurement.
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

The normal headless unit test
`tests::runtime_diagnostics_classify_monitor_events_without_a_native_viewer` drives synthetic
decoder events through the production `App::apply_monitor_decode_event` classifier. It proves exact
stale/non-converging drop, late, hold, presentation, upload-mode, and current-error deltas plus
`holds <= late` and `presented = native + fallback`. Its test-only constructor skips startup-resource
loading and native audio initialization. Because the test has no native renderer, it expects the
observed fallback-upload path and does not claim GPU presentation or physical scanout. Packaged and
sustained runners remain the numeric runtime threshold gates.

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

The editor's normal title-bar HUD hover reuses these same runtime-owned aggregates for a compact
English/Japanese live view. Decoder and audio rows display mean/maximum values; bounded viewer and
GPU rows display p95/maximum values; every observed row includes its sample count. The table also
shows active contributing video layers and selected-to-resolved preview quality. A compositor CPU
snapshot uses a non-blocking lock attempt, so diagnostics never wait for the render callback.
Unsupported GPU timestamp queries, a busy callback snapshot, and stages with no samples display as
unavailable rather than a fabricated zero. The sparse named-decoder-reopen stage remains report-only
and is not added to the compact live HUD. This live view does not change the schema-9 report or
its qualification role.

Facts unavailable through a supported platform API are serialized as JSON `null`; zero and
`"Unknown"` are not used as hidden unavailable-value sentinels.

## Phase 0 timeline foundation qualification

`scripts/Run-TimelineFoundation.ps1` turns the existing release performance tests into one
repeatable, versioned gate. It runs ten independent 50,000-clip history invocations by default,
the wide/detail/playhead CPU test, and the combined real H.264 decode plus 20,002-bar interaction
test. The input must be an absolute positive-duration H.264 file; its path is never serialized.
The report retains only its SHA-256, size, codec, dimensions, rate, and duration.

Run from a clean tracked workspace with the pinned runtime:

```powershell
& 'H:\Maelstrom Rust\scripts\Run-TimelineFoundation.ps1' `
  -MediaPath 'C:\absolute\path\to\qualification-h264.mp4' `
  -FfmpegRoot 'H:\Maelstrom Rust\.deps\ffmpeg-project-8.1'
```

The runner resolves Cargo and Git to absolute executable paths, routes every Rust test through the
repository Cargo runner, requires an unchanged clean tracked commit, and atomically writes schema 1
evidence to `artifacts/phase0-foundation/timeline-foundation.json`. A passing report is headless
release CPU/decode evidence only. It does not establish GUI-present input latency, GPU completion,
physical input latency, DWM/scanout, packaged smoke, or cross-hardware performance.

The retained local schema-1 run on clean commit
`6576d91e25d34f8a3203382d9bd483ffe9e77056` passed ten independent history trials with
0.2672/0.7453 ms press/edit-release p95, 0.4578/0.2900/0.2787 ms wide/detail/playhead p95, and
0.4839 ms p95 for the real 1920×1080 H.264 plus 20,002-bar case. The report contains no private
media path. Its SHA-256 is
`9D69B9EA33F0E621E47AE31C04B32BC3345343CDBA3E1C491F6143B7340205E2`.

The gate was renewed on 2026-08-31 at clean commit
`4f4fd5e6eeb7cee6b09644cb9376954bf2e47dfd`. Ten independent 50,000-clip history trials passed
with 0.2565/1.2552 ms press/edit-release p95; wide/detail/playhead CPU p95 was
0.4885/0.3386/0.4274 ms; the private real 1920x1088 H.264 plus 20,002-bar case was 0.5108 ms p95.
The media path is omitted. The renewed report SHA-256 is
`36675C22C160CD4A0B90EBAB89DA33F859B50046B0CFCF204BF7FD161DE49399`.

## Phase 0 cross-adapter compositor qualification

The headless DX12 qualification exercises the production `ViewerCompositorRenderer` offscreen,
not the editor, window surface, DWM, or a display. It explicitly enumerates one
`IntegratedGpu` and one `DiscreteGpu` adapter. Schema 3 uses pre-uploaded 1920x1080 sources and a
1920x1080 output with Bicubic sampling: two transformed layers on the integrated adapter and four
on the discrete adapter. Five warmups are excluded, followed by 30 changed generations with CPU
encode and required `TIMESTAMP_QUERY` GPU-pass timing. A deterministic center-pixel readback must
match the expected RGBA within four channel values. Run it only through Cargo via the repository
runner:

```powershell
& 'H:\Maelstrom Rust\scripts\Run-Phase0CrossAdapterGpu.ps1'
```

After the timed workload, schema 3 runs four ordered state transitions on the same retained
renderer. It moves only the top-layer transform away from the center, disables the top layer,
restores its declaration while removing its retained texture to model a missing source, then re-uploads it
to model late arrival. Every transition advances the frame generation and verifies center RGBA,
current upload/composition serial relationships, top-layer participation, and whether an upload
was permitted. The transform transition additionally probes inside the moved quad, so it must show
the remaining stack at center and the full stack at the transformed location. The transform and
disable transitions must not upload. The missing transition must keep every ready lower source
composed. The late transition must allocate a newer upload serial and restore the full expected
composition.

The atomically written ignored local evidence is schema-version 3 JSON at
`artifacts/phase0-cross-adapter/phase0-cross-adapter-gpu.json`. It records machine identity and
adapter name/vendor/device/type/backend/driver information, workload dimensions, sampling mode,
layer count, warmup/measurement counts, actual and expected RGBA, and CPU/GPU p95 and maximum.
The runner enforces CPU p95 at or below 8 ms and GPU-pass p95 at or below the 33.333 ms 30 fps
budget. It deliberately records `physical_scanout_observed: false` and
`app_auto_preview_observed: false`, and is limited to
`scope: "headless_transformed_multilayer_viewer_compositor_with_post_measurement_state_scenarios"`:
it does not replace the schema-9
surface report or establish app Auto resolution, presentation, DWM composition, physical scanout,
or end-to-end display latency.

The 2026-08-31 schema-3 qualification passes on Intel UHD 770 with two layers and RTX 3090 with
four layers. Both adapters pass all four state readbacks and serial invariants while staying within
the unchanged CPU and 30 fps GPU budgets. The file is regenerated on every run, so its
timing-dependent SHA-256 and individual timing samples are intentionally not stable documentation
identifiers.

The schema-3 file remains the success payload. On runner/preflight, Cargo, or schema-3 validation
failure, the runner atomically replaces the requested output with a distinct schema-1 failed-run
envelope scoped to `headless_cross_adapter_viewer_compositor_qualification`. It keeps unavailable
machine/adapter/backend/driver fields null, declares both presentation observations false, and
contains a bounded primary failure. `-ValidateOnly` performs only path, dependency, and contract
checks without changing evidence. The test-only paired
`-ValidateOnly -FailureReportContractFixture` seam validates failed-envelope publication, including
a colliding sibling `.tmp` path; it creates no Cargo, GPU, FFmpeg, or GUI activity. This is harness
support only, not new runtime or hardware evidence.

## Phase 2 integrated Auto-preview qualification

The schema-1 integrated-Auto gate joins the real app monitor scheduler to the retained compositor
without opening the editor. It consumes the first two independently generated Phase 1 hue fixtures,
verifies both inputs remain 1920x1080, selects Auto in `EditorState`, and accepts current real
decoder events for both layers at Full 640x360. Four deliberately old request-start timestamps are
then supplied immediately before the production completion-application path. This controlled input
exercises the real four-sample Auto hysteresis but is explicitly not an organic decode-latency
measurement.

After Auto resolves to Half, the test advances the target and requires both layers to receive newer
generation and request IDs, decode fresh 320x180 frames, and retain the correct independent media
identities. It then selects an exact DX12 `IntegratedGpu` adapter with no fallback, uploads those
two decoded frames to `ViewerCompositorRenderer`, places them in non-overlapping transformed halves,
and checks a source-specific readback in each half. Upload and composed-upload serials must match.
The qualification-only renderer bridge is enabled only for the `nle-app` test dependency and is not
present in normal production builds.

Run the gate through the full repository script path:

```powershell
& 'H:\Maelstrom Rust\scripts\Run-Phase2IntegratedAuto.ps1'
```

The runner regenerates and validates the shared fixtures, holds their named mutex throughout the
decode/compositor run, and fails closed on source path/size/resolution, schema types, adapter class,
request identities, quality/dimensions, decoder backend, probe coordinates/RGBA/tolerance, and
upload/composition serials. Its ignored report is
`artifacts/phase2-integrated-auto/phase2-integrated-auto.schema1.json`.

The retained 2026-08-31 run selected Intel UHD Graphics 770 through DX12. Auto advanced both real
layers from Full 640x360 to Half 320x180; both transformed source probes passed with the fixed
24-channel tolerance. The report records
`scope: "headless_app_auto_scheduler_and_integrated_compositor"`,
`window_surface_observed: false`, and `physical_scanout_observed: false`. It therefore closes the
Phase 2 app-Auto/integrated-compositor gate, not native viewer presentation, DWM composition,
physical scanout, or end-to-end display latency.

## Phase 0 cross-adapter full surface qualification

The opt-in full-surface runner complements the headless compositor proof. It launches the exact
packaged editor once on a compatible DX12 `IntegratedGpu` and once on a compatible DX12
`DiscreteGpu`, using the ordinary window surface, media import, native viewer upload, audio
callback, and cancelled Quick Export acceptance path. Each run must produce a complete
schema-version 9 report with exercised decoder, viewer, GPU, audio, and runtime-diagnostic fields
while retaining the foundation CPU and surface-cadence limits. Normal editor startup remains
unchanged; the adapter-class override exists only through the explicit
`MAELSTROM_PHASE0_SURFACE_ADAPTER_CLASS` qualification seam and fails rather than falling back to a
different adapter type.

Run it only through the exact project launcher:

```powershell
& 'H:\Maelstrom Rust\scripts\Run-Phase0CrossAdapterSurface.ps1' `
  -LauncherPath 'H:\Maelstrom Rust\Launch-Maelstrom-Editor.bat'
```

The ignored `artifacts/phase0-cross-adapter-surface` directory retains the most recent surface
reports, startup reports, media-acceptance reports, and a schema-version 2 wrapper containing the
exact launcher and derived executable hashes plus every completed child-report hash. The runner
derives the package executable only for runtime/identity checks, starts the batch launcher through
`ComSpec /d /c call` with `MAELSTROM_LAUNCHER_WAIT=1`, finds only the exact packaged editor child
inside that fresh launcher PID tree, and terminates only that launcher-rooted tree in cleanup.
`-ValidateOnly` checks the fixed launcher, derived package/runtime inventory, and both hashes
without launching the editor, FFmpeg, or FFprobe and without changing retained evidence. Surface schema 9 retains the nested
`observation_scope`: submission, present-call CPU, and completed GPU submissions are independently
reported, while `physical_scanout_observed` remains false until supported instrumentation exists.
It also requires the additive `named_decoder_reopen` stage, without requiring nonzero samples in an
ordinary smoke run. Hardware-transfer timing is validated against the exact serialized decoder
backend identities: Apple VideoToolbox, Windows D3D11VA, and Windows DXVA2 require transfer samples;
Software, Intel Quick Sync, and NVIDIA CUVID may correctly report zero. Unknown or blank identities
and malformed timing aggregates are rejected. This is validator-only contract coverage, not new
runtime or hardware evidence. The preceding retained cross-adapter evidence is schema 7; schema 8 was never
requalified before schema 9 superseded it. A fresh explicitly authorized editor run is therefore
required before the current surface contract can be qualified. A pass has `failure: null`. Once the
report destination has been validated and the exclusive run lock acquired, the runner attempts to
atomically publish `status: "failed"` and one structured `failure` object before returning a
nonzero result. A unique same-directory temporary file prevents stale fixed-temp collisions. That
object records a stable component and stage, requested adapter class, only the codecs affected by
that stage, any renderer/decoder/encoder backend and driver data already observed, a bounded
message, relevant artifact path, and process exit code when available. Unavailable backend or
driver values remain JSON `null`; the runner never infers them from an adapter request. The wrapper
also records the deterministic fixture codecs separately (`mpeg4` video and `aac` audio).

### Prepared schema-9 package checkpoint — 2026-09-01

The portable package was rebuilt from clean source commit
`c3cfebe856a42b7c8566a6f7b8cfa21b55b5fd7a` through the reviewed
`scripts/package-windows.ps1 -SkipSmoke` path. The packaged `Maelstrom.exe` exactly matches
`target\release\nle-app.exe` at SHA-256
`27BC9D24B2DD921607781BFE3B5DFD4FBE7574A7BDB2CACFD45F081171DF6DF8`. Its 23-file package is
runtime-complete: all 13 copied FFmpeg command/runtime inventory hashes match the pinned project
bundle, the app-local `vcruntime140.dll` matches the authorized installed Visual Studio AMD64 CRT,
and the packaged model manifest matches its documented source. All 15 PE files report AMD64; a
read-only `dumpbin /dependents` walk classified all 117 static import edges as 43 adjacent,
62 present Windows-module, and 12 API-set-contract edges, with no unresolved non-contract
dependency. API-set contracts are virtual aliases and were classified rather than resolved by this
static inspection. Separate post-build checks prove packaged FFmpeg and FFprobe load using a
restricted package-plus-Windows path and report `n8.1-maelstrom-20260824`. The exact
`H:\Maelstrom Rust\Launch-Maelstrom-Editor.bat --verify-runtime` branch also passes and returns
before its launch branch.

Before replacement, the previous 23-file package was copied to
`H:\Maelstrom Rust\dist\package-backups\Maelstrom-Windows-x64-pre-schema9-C42458E86AE9` and every
relative-path hash was verified. Only `Maelstrom.exe` and `PACKAGE-STATUS.json` differ between that
copy and the rebuilt package. No DLL was downloaded or installed. `PACKAGE-STATUS.json` deliberately
records `smoke_status: "not_run"`; no editor, GUI surface, schema-9 runtime report, export smoke, or
physical scanout was exercised. Historical schema-7 reports do not qualify this executable. A fresh
schema-9 windowed run remains permission-gated to the exact full-path launcher. Current static
evidence is retained locally at `artifacts/package-schema9-c3cfebe/verification.json`: 3,378 bytes,
SHA-256 `1F4F33952A321722788D728C7804C3E85C242E8A87F3EC7203528ACBF97249B7`.

The focused failure-contract check uses a validation-only, fixed-path synthetic runtime-closure
seam (it cannot supply a package path or launch the editor) and verifies the schema-2 `package` /
`runtime_closure` diagnosis:

```powershell
& 'H:\Maelstrom Rust\scripts\Test-Phase0CrossAdapterFailureReport.ps1'
```

The launcher contract check also verifies non-launching validation preserves an existing ignored
artifact and rejects the packaged executable when presented as a launcher:

```powershell
& 'H:\Maelstrom Rust\scripts\Test-Phase0CrossAdapterLauncherContract.ps1'
```

The wrapper deliberately records `physical_scanout_observed: false`: a successful surface present
and wgpu completion still do not prove DWM composition or physical scanout.

The retained 2026-08-30 hybrid-host run predates the failure-envelope revision and passed on Intel
UHD 770 `IntegratedGpu` and NVIDIA RTX 3090 `DiscreteGpu`. Its schema-version 1 summary SHA-256 is
`d1bd17bb3c482de9c7d26c8dc507ff5096961656d0998fa4d9697ccb6541e385`; the qualified packaged
executable SHA-256 is `0f81e9e9df349c9f3b3254cdcf6d891b9cf0fc6faf91f33f4236557df3a44ad0`.

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

Run the full Phase 0 gate only through the exact project launcher:

```powershell
& 'H:\Maelstrom Rust\scripts\Run-PlaybackSoak.ps1' `
  -LauncherPath 'H:\Maelstrom Rust\Launch-Maelstrom-Editor.bat' `
  -DurationSeconds 600
```

For a quick plumbing check, pass an explicit short duration such as `-DurationSeconds 15`; that is
not evidence for the ten-minute gate. `-ValidateOnly` performs no GUI launch while proving the
exact launcher path plus the derived packaged editor, sibling `ffmpeg.exe`, `ffprobe.exe`, runtime
inventory, and launcher/editor hashes. A real run derives the package path only for runtime and
report identity, builds a deterministic 60-second A/V clip, then starts the editor exclusively by
calling `H:\Maelstrom Rust\Launch-Maelstrom-Editor.bat` with `MAELSTROM_LAUNCHER_WAIT=1`.
During the opt-in run the editor window remains visible and always-on-top to avoid Windows
occluded-surface throttling; it returns to normal level when the application report is written.

The app atomically writes schema-version 5 `playback-soak-app-report.json` under the ignored
`artifacts/phase0-playback-soak` directory. It contains requested and actual wall duration,
completed loop count, observed decoder backends, selected/resolved preview quality, configured
monitor-cache cap, `monitor_resources`, and deltas for monitor
requests/completions/presents/drops/holds/lates/errors, rolling request-turnaround p95,
native/fallback viewer uploads, audio underruns, callback lock failures, and late audio discards.
It also records the cumulative decoder stage-timing aggregates so a sustained regression can be
separated into cache, demux, decoder-call, transfer, scaling, RGBA-pack, or whole-worker cost
without per-frame logging. `monitor_resources` reports the one
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
underruns/callback lock failures/late discards. It then atomically writes schema-version 2
`playback-soak-report.json` beside the app report. A passed wrapper has `status: "passed"` and
`failure: null`; it records the exact launcher path/SHA-256, derived executable path/SHA-256, and
coarse WorkingSet64 evidence.
After the artifact directory and stale-artifact cleanup have succeeded, every runner failure
attempts to atomically publish the same schema with `status: "failed"`, the exact current
`failure.stage`, exception type/message,
any executable identity available at that point, the parsed application report when available, and
the bounded WorkingSet64 samples collected so far. Failure stages distinguish `path_validation`,
`packaged_runtime`, `fixture_generation_codec`, `editor_launch_report_wait`,
`app_report_schema_environment`, `app_report_resources`, `app_report_runtime_diagnostics`, and
`report_publication`. The runner rejects a non-absolute launcher path or any launcher other than
`H:\Maelstrom Rust\Launch-Maelstrom-Editor.bat`, derives and verifies the packaged `Maelstrom.exe`,
`ffmpeg.exe`, `ffprobe.exe`, and the same app-local DLL inventory required
by Windows packaging before launch, and refuses to launch if a stale report or temporary report
cannot be removed after bounded retries. Failure evidence does not relax any acceptance limit.

WorkingSet64 is sampled about once per second from only the exact derived packaged GUI child found
under the tracked launcher process tree, never from `cmd.exe` or by process-name search. Its warm
baseline is the third sample, and the final report records peak/final/growth plus
a deliberately generous 1.5 GiB peak-growth bound. This is not total system memory, GPU memory,
or child-process memory, so it detects only coarse sustained GUI working-set growth rather than a
complete leak diagnosis. The runner always restores its environment and terminates only its tracked
PID tree in `finally`.

The latest 2026-08-30 Windows checkpoint passed against packaged executable SHA-256
`4738277EA7942107634BDC01A6974CE749041A391A9BEDFDE85AF1C0720B406B`: 600.003 seconds,
10 timeline loops, `Full` selected/resolved quality, 18,023 native viewer uploads, zero held/late
frames, zero monitor errors/fallback uploads, and zero audio underruns/callback lock failures/late
discards. The rolling request-turnaround p95 was 12.242 ms. The cumulative stage aggregates
recorded 18,066 worker requests averaging 6.148 ms and 37,374 measured decoder calls averaging
0.0065 ms.
The shared decoded-frame cache ended at 1,067,996,160 bytes and peaked at 1,071,555,584 bytes below
its 1,073,741,824-byte cap. One foreground session/actor remained active, speculative background
ownership was zero, and the exact session/actor bounds held. Peak GUI working-set growth above the
warmed baseline was 1,038,934,016 bytes, within the deliberately generous 1.5 GiB bound. The
ignored local schema-version 2 wrapper and schema-version 5 app evidence remain in
`artifacts/phase0-playback-soak/`; their SHA-256 values are
`A1B4A1F485D93B3846E19CF87655CE76D40E4CD533489137607E1998796DB175` and
`7431EFA650A1BE4B4262795E76CBD6C80EF585D95EC430BDD66C56D6C41682DD` respectively.

## Opt-in packaged playback disruption schedule

`scripts/Run-PlaybackDisruptions.ps1` is the separately opt-in live GUI schedule for disruptions
that a normal soak intentionally does not cause: eight runtime scrub updates, eight snapshot/project
restores that each must present a newly owned frame, an intentional missing-media decoder error
followed by exposed offline state and recovery playback, sequential Full-quality decoded-frame cache
pressure, and a started/progressed/cancelled export with terminal cleanup.

```powershell
& 'H:\Maelstrom Rust\scripts\Run-PlaybackDisruptions.ps1' `
  -LauncherPath 'H:\Maelstrom Rust\Launch-Maelstrom-Editor.bat'
```

The real run creates an owned 60-second 1920x1080 A/V fixture with package-local FFmpeg and starts
only the exact project launcher, passing `--cache-mb=512`. It supplies the smoke-editor, media,
report, export, and launcher-wait environment values; the runner finds only the exact packaged editor
descendant under that fresh launcher root and cleans only that PID tree. The schema-1 app report must
show exactly eight scrubs/restores/restore frames and frame-gated playback/audio-transport restarts;
decoder/offline/recovery evidence; positive cache
eviction growth; export start, progress, cancellation, and cleanup; `Full` selected/resolved quality;
bounded diagnostics; and exactly one monitor error from the intentional missing source. The additive
schema-1 wrapper records launcher/executable SHA-256 values and structured stage/type/message failures.
It publishes `status: "passed"` only after the captured launcher/editor/descendant PID set is gone,
the original environment is restored, and every disposable owned media/export/`.part`/temporary
artifact has been removed. Cleanup failure makes the invocation fail and writes `status: "failed"`
with the original operational failure (if any) plus every cleanup error; app and final reports are
retained as evidence. Success additionally requires `cleanup_timeout: false`,
`export_job_settled: true`, and `export_residue_present: false`.

`-ValidateOnly` proves exact launcher/package/runtime identity without creating/deleting artifacts,
changing environment variables, running FFmpeg, or launching the GUI. Its contract rejects direct
executable paths. The harness is installed but has not produced live, cross-hardware, scanout, or new
performance evidence.
