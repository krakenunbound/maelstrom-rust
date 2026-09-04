# Maelstrom TODO

Short operational queue for active development. `ENGINE_ROADMAP.md` is the authoritative feature,
architecture, performance, and exit-gate specification. `MAELSTROM_ACTION_PLAN_OVERLAY.md` provides
supplemental sequencing and constraints. If this list conflicts with either document, follow the
roadmap and correct this file.

Last updated: 2026-09-04

## Current stopping point

- [x] Refresh the portable Windows package from clean commit `d6f6271` after the shifted
  Matroska-duration fix and current Phase 0 fixture work. The recoverable previous 23-file package
  is hash-verified at
  `dist\package-backups\Maelstrom-Windows-x64-pre-d6f6271-0FC9345A514A`; only
  `Maelstrom.exe` and `PACKAGE-STATUS.json` changed. The new executable matches the release build
  at SHA-256 `37F7EB9AABCE560ACAC3D164C00439710436A8BA004551718A1A42D1F7AA108D`.
  All 13 pinned FFmpeg runtime copies and authorized supporting files match their sources, all 15
  PE files are AMD64, and 115 static import edges resolve as 43 adjacent, 60 Windows modules, and
  12 API-set contracts with none unresolved. Restricted-path FFmpeg/FFprobe loading and the exact
  full-path launcher's `--verify-runtime` branch pass. No editor was launched; `smoke_status`
  remains `not_run`, so schema-9 windowed qualification still requires explicit launch permission.
  Package assembly now completes in a GUID-named sibling staging directory before activation;
  move-with-rollback preserves the prior live package on an injected activation failure, and
  incomplete staging is cleaned without deleting an unrestored rollback. Focused activation and
  existing launcher-contract tests pass, followed by a complete staged `-SkipSmoke` rebuild.
- [x] Rebuild the portable Windows package from clean commit `c3cfebe` with the schema-9
  surface-report contract, using the supported `-SkipSmoke` path. The packaged executable matches
  `target\release\nle-app.exe` at SHA-256
  `27BC9D24B2DD921607781BFE3B5DFD4FBE7574A7BDB2CACFD45F081171DF6DF8`. All 23 files are present;
  pinned runtime/model/VC hashes match their approved sources; all 15 PE files are AMD64; 117 static
  import edges classify as 43 adjacent, 62 present Windows-module, and 12 API-set-contract edges,
  with no unresolved non-contract dependency. Separate restricted-path FFmpeg/FFprobe checks and
  the exact full-path launcher's `--verify-runtime` check pass. Only the executable
  and `PACKAGE-STATUS.json` differ from the verified 23-file recovery copy. No GUI was launched and
  `smoke_status` remains `not_run`; fresh schema-9 windowed qualification still requires explicit
  launch permission. See `docs/performance-reports.md`.
- [x] Extend reproducible Windows hardware-VFR qualification through AV1 without launching the
  editor. The schema-1 runner at clean commit `4965317` verifies the complete 42-file pinned FFmpeg
  bundle, then passes D3D11VA, DXVA2, NVIDIA CUVID, and Intel Quick Sync for H.264 High, HEVC Main
  10, and AV1 Main at 64x48 and 1920x1080: 456 exact timestamp-and-pixel comparisons with no
  fallback. All four CLI paths also match AOM's published decoded-frame MD5 sequence. The ignored
  AV1 fixture is reproducibly stream-copied from checksum-pinned local AOM inputs; neither inputs
  nor derivative are redistributed. QSV reverse seeks retain exactly seven named-decoder reopens
  per output size. This is host/backend evidence, not physical-adapter, GUI, scanout, export-parity,
  or universal no-lag proof. See `docs/media-fixtures.md` and `docs/hardware-decode-parity.md`.
- [x] Preserve shifted AV1 VFR presentation timing through media analysis and app scrub routing.
  The bounded decoded-frame scan now retries `av1_cuvid` then `av1_qsv` only when the default AV1
  decoder yields no usable timing; it never substitutes packet order for presentation frames.
  The local AOM-derived fixture publishes all eight normalized boundaries and the app proves
  bidirectional floor/hold routing. This is local named-decoder evidence, not arbitrary AV1 or
  physical-adapter conformance. See `docs/media-fixtures.md`.
- [x] Preserve shifted AV1 VFR source identity through export. The selected video codec is probed,
  AV1 inputs retry bounded D3D11VA/DXVA2/CUVID/QSV/default decoder paths without changing non-AV1
  jobs, and failures name both decoder and encoder. Ten head/trim/slip/tail/final-frame cases at
  30 and 30000/1001 fps match preview across 46 exported frames. The local fixture now uses eight
  distinct official AOM all-intra pictures rather than weak quantizer variants. Phase 0, waveform,
  app routing, and all four Windows AV1 hardware seek paths pass locally. The clean-source hardware
  wrapper at `f76c9ab` passes all 12 backend/codec tests and 456 exact cases against the stronger
  fixture. See `docs/shifted-vfr-export-parity.md` and `docs/hardware-decode-parity.md`.
- [x] Persist the user's per-media proxy enable choice without persisting derived media. Project
  schema 9 stores one defaulted boolean; v1–v8 migrate disabled, while export and duplicate preserve
  intent. Reopen keeps original routing until current cache validation succeeds, and explicit
  disable/delete, relink, project switch, or late results cannot reactivate it. See
  `docs/proxy-media.md`.
- [x] Extend shifted-VFR preview/export source-identity parity to the reordered MPEG-4 MP4. Ten
  head/trim/slip/tail/final-frame cases at 30 and 30000/1001 fps add 44 exact identities, bringing
  the three-fixture production-graph gate to 30 cases and 132 exported frames. No production graph
  change was required. See `docs/shifted-vfr-export-parity.md`.
- [x] Add a deterministic shifted/reordered MPEG-4 VFR MP4 to the Phase 0 fixture contract. The
  eight-frame file combines a three-second source origin, irregular gaps, and I/B/B/P/B/B/P/P
  packet reordering. Its generated output matches the pinned hash/size; manifest/waveform/decode/app checks
  pass, including 19 exact Software/CLI pixel seeks and bidirectional preview floor/hold mapping.
  The complete seven-scenario Phase 0 runner passes without launching the editor. See
  `docs/media-fixtures.md`, `docs/codec-color-qualification.md`, and `docs/phase0-scenarios.md`.
- [x] Add deterministic multichannel audio to the Phase 0 corpus. The generated one-second PCM WAV
  pins six distinct 5.1 channels, layout, duration, byte size, and hash. Manifest-only validation
  now requires mono, stereo, and multichannel classes and proves missing multichannel coverage is
  rejected without touching files or running FFmpeg. This is corpus evidence, not mixer/export parity.
- [x] Strengthen the finite Phase 0 rapid editor-state-switch scenario. Its eight headless Software
  switches retain one completed real frame, then hold a distinct real 160x90 request in flight at a
  test-only decoder worker boundary and switch before consuming it. Production cancellation must suppress
  that in-flight request and quiesce ownership; the retained prior-generation event is separately rejected
  without pixel/offline/error changes. Each switch then presents a fresh active-project event with the
  expected media/path/playhead identity. The report requires exactly eight cancellation suppressions,
  stale prior-generation rejections, and fresh presentations, plus generation and media-analysis epoch
  advancement and zero post-release session/source/live- and retiring-actor ownership. This is delayed-
  event/session-lifecycle evidence only: it does not claim GUI/audio/hardware/scanout, playback quality,
  or broader cross-hardware qualification.
- [x] Add a deterministic RGBA PNG still-image fixture to the Phase 0 corpus. FFprobe proves only
  PNG codec/container on the pinned build; direct PNG validation proves the 160×90 8-bit RGBA
  IHDR/chunks/alpha coverage, byte size, and hash. The manifest-only negative contract proves
  required image metadata cannot be omitted. This is corpus evidence only, not still-image import,
  alpha compositing, preview, or export parity.
- [x] Add bounded deterministic 4K video corpus coverage. The generated four-frame 3840×2160/24
  MPEG-4 fixture pins yuv420p, two-frame GOP/keyframes, duration, size, and hash; ManifestOnly
  rejects removal of all 4K-class video in memory. It is not part of default long soaks and makes
  no playback-quality, package, decode-performance, or hardware claim.
- [x] Add bounded deterministic 8K video source coverage. The generated two-frame 7680×4320/24
  MPEG-4 fixture pins identity and accepts exactly two explicit software/no-hwaccel decoded frames
  to a null sink; it establishes no playback, export, GPU, throughput, scanout, or long-soak claim.
- [x] Pin the local-only H.264 High long-GOP scrub input beneath an explicit real-media corpus
  root. Its filename, hash, size, stream, duration, frame/keyframe, and I/P/B evidence are
  validated only when opted in; it is input to the separately run Software scrub test, not a scrub
  test itself. The file remains outside Git and establishes no hardware, package, playback-quality,
  or performance evidence.
- [x] Pin the local-only HEVC Main 10 BT.709 source under the same corpus root. Its identity,
  color, frame/keyframe, and I/P/B evidence are validated only when opted in; recorded QSV encoder
  intent is provenance, not proof. It is input to the separately run Software HEVC seek test and
  establishes no GPU, package, playback-quality, or performance evidence.
- [x] Make Phase 0 surface-observation limits machine-readable. Surface report schema 9 now
  distinguishes observed surface submission, present-call CPU time, and completed GPU submissions
  from physical scanout, which remains explicitly false, and carries the optional named-decoder
  reopen timing stage. Package and cross-adapter validators reject missing, contradictory, or
  overstated scope. The preceding Intel/NVIDIA schema-7 runs remain historical evidence; schema 8
  was not requalified before being superseded. A fresh schema-9 windowed run still requires explicit
  editor-launch permission. See `docs/performance-reports.md`.
- [x] Make Phase 0 hardware-transfer timing evidence backend-aware. Shared package and
  cross-adapter validation now accepts zero samples only for observed CPU-readable serialized
  backends and rejects unknown identities or contradictory/malformed aggregates. The synthetic
  PowerShell contract adds no runtime or hardware evidence.
- [x] Share the schema-9 surface-timing validator between package and cross-adapter runners.
  Its headless contract covers supported/unsupported GPU timestamp-query reports, required timing
  groups, aggregate math, and the explicit no-scanout boundary; runtime sample and threshold gates
  remain caller-owned. This adds validator coverage only, not Phase 0 qualification evidence.
- [x] Close the Phase 2 integrated Auto-preview gate without lowering the user's selected quality.
  A schema-1 headless app test uses two independent real 1920x1080 sources, observes Auto Full
  640x360, applies four disclosed controller-pressure samples through the production completion
  path, then proves both layers resubmit with newer identities and return real Auto Half 320x180
  frames. The exact DX12 Intel UHD 770 compositor uploads both frames and passes two independent
  transformed readbacks with matching upload/composition serials. The test-only renderer bridge is
  absent from production builds. This proves app Auto scheduling plus integrated GPU composition,
  not organic decode pressure, native-window presentation, DWM, or physical scanout. See
  `docs/performance-reports.md`.
- [x] Prove next-generation layer-state changes across both reference GPUs. The schema-3 headless
  1080p compositor gate retains its timed Bicubic workload, then changes only the top transform,
  disables that layer, removes its retained source texture, and re-uploads it as a late arrival.
  Exact center plus moved-quad readbacks and upload/composition serials prove no-upload
  transform/disable recomposition, ready
  lower-layer output while the source is missing, and full restoration when it arrives. The
  renewed four-source real-media stress simultaneously passed 32 disable/re-enable/backward-scrub
  cycles with one blocked request superseded, 39 stale events rejected, zero monitor errors, and
  bounded teardown. App Auto is now separately qualified headlessly; window presentation, DWM and
  scanout remain separate. See
  `docs/performance-reports.md` and `docs/phase1-generation-stress.md`.
- [x] Qualify the retained compositor at full 1920x1080 on both reference adapters. The schema-3
  headless DX12 gate pre-uploads its sources, excludes five warmups, then measures 30 changed
  generations using Bicubic sampling and verifies the center pixel. Intel UHD 770 passed two
  transformed layers at 0.1997 ms CPU / 6.9053 ms GPU p95; RTX 3090 passed four transformed layers
  at 0.0904 ms CPU / 0.2765 ms GPU p95. This closes the discrete four-layer compositor gate and
  the integrated two-layer full-1080p performance prerequisite. App Auto scheduling and integrated
  composition are now separately qualified at Full 640x360 to Half 320x180; presentation, DWM and
  physical scanout remain unproven. See `docs/performance-reports.md`.
- [x] Add failed-run evidence support to the headless cross-adapter compositor harness. Runner,
  preflight, Cargo, and report-validation failures atomically publish the distinct schema-1
  envelope while successful schema-3 evidence remains unchanged. The non-launching fixture proves
  collision-safe publication only; it adds no runtime or hardware qualification evidence.
- [x] Renew the clean-HEAD Phase 0 timeline-foundation evidence at `4f4fd5e`. Ten 50,000-clip
  history trials passed at 0.2565/1.2552 ms press/edit-release p95; wide/detail/playhead CPU p95 was
  0.4885/0.3386/0.4274 ms; real H.264 plus 20,002 bars was 0.5108 ms p95. The ignored report hash is
  `36675C22C160CD4A0B90EBAB89DA33F859B50046B0CFCF204BF7FD161DE49399`; GUI, package, scanout and
  cross-hardware evidence remain outside this gate. See `docs/performance-reports.md`.
- [x] Renew the clean-HEAD Phase 0 timeline-foundation evidence at `57277e6`. All ten 50,000-clip
  history trials passed the unchanged 2 ms press/edit-release checks with 0.3103/0.7599 ms p95;
  wide/detail/playhead CPU p95 was 0.5626/0.3383/0.3822 ms, and real 1920x1088 H.264 plus 20,002
  bars was 0.4358 ms p95 through Software decoding. The report omits the private path and has
  SHA-256 `AE41C32043A3CDA34EA3B944EC292D461B5ECABF4F1A86A3E7B408E7E8C0A608`.
  This is headless CPU/decode evidence only; GUI, package, GPU, physical input/scanout, and
  cross-hardware proof remain open. See `docs/performance-reports.md`.
- [x] Remove Windows fixture-deletion races from the coordinated-decoder diagnostic and
  actor-budget tests. Teardown now waits for the asynchronous source reaper, live source
  ownership, and sticky FFmpeg session count to reach zero before deleting temporary files.
  Twenty focused release repetitions of each repaired path and ten consecutive complete decoder
  suites pass; the current serial release workspace reports 910 passed and 36 opt-in ignored,
  together with strict all-target Clippy and formatting. No editor was launched.
- [x] Add independent Nearest/Bilinear/Bicubic preview sampling with a Bicubic default and nested
  EN/JA Playback menu. The persisted preference migrates the former high-quality boolean and is
  part of monitor request, cache, sticky scaler and hardware-transfer identity without changing
  playback resolution or export. FFmpeg uses explicit point/bilinear/bicubic filters; the retained
  viewer uses nearest/linear samplers or a manual alpha-safe Catmull-Rom shader and recomposes an
  existing upload when the setting changes. Focused migration/cache/WGSL tests, the full release
  workspace, strict Clippy and a real-GPU bicubic edge/readback gate pass. Practical four-layer 4K,
  physical scanout and broad cross-adapter performance remain open. See
  `docs/preview-sampling.md`.
- [x] Bound project-monitor resource churn with exact-size layer/output reuse. The free pool shares
  a 32 MiB logical-payload cap, four-layer and one-output-pair entry limits, oldest eviction,
  oversize rejection, and a full-clear purge. Fixed CPU command scratch is retained; the compositor
  creates no command buffers because the render callback supplies its encoder. A real GPU gate
  queues resize, visibility-clear and temporary-no-frame reuse without per-submit waits, then
  verifies the final output by readback. Physical VRAM, memory pressure, practical 4K pooling and
  cross-adapter performance remain open. See `docs/compositor-resource-pool.md`.
- [x] Correct transparent-edge handling across the retained viewer compositor and generated export
  graphs. Straight RGBA remains the asset/FFmpeg-overlay contract; changed viewer uploads receive a
  retained encoded-sRGB premultiply pass, media/mattes use premultiplied source-over, and sRGB plus
  non-sRGB presentation surfaces preserve the same encoded result. Export premultiplies only around
  filtered transforms and restores straight alpha before overlays. Real GPU and pinned-FFmpeg
  regressions cover filtered edges, both presentation formats, generated stills, transforms,
  cross-dissolves, titles and mattes. Texture pooling, memory-pressure, cross-adapter and Phase 4
  linear-working-space gates remain open. See `docs/premultiplied-alpha.md`.
- [x] Requalify the corrected four-source live-audio continuity gate for the maximum
  30-second duration at tracked-clean commit `f78c816`. The local Software-backend run
  sustained four Full-1080p sources through a real default output device, including a
  750 ms blocked topmost source. Input submission p95 was 46 us, clock drift was 7,138 us,
  and audio/monitor fault counters remained zero. This is not GUI, packaged-playback or
  cross-hardware evidence. See `docs/phase1-live-audio.md`.
- [x] Requalify the full 600-second Phase 0 seven-scenario soak at clean commit `773dd92`.
  The authoritative schema-2 wrapper passed 76 complete matrices (532 scenario executions,
  2,812 declared work iterations) over 607.115 measured seconds. All child report hashes are
  unique; start/end commits match; no tracked changes, invocation/report failures or leftover
  processes were observed. Headless Software decode/`h264_mf` evidence only; cross-hardware,
  live audio, GUI and scanout gates remain open. See `docs/phase0-scenarios.md`.
- [x] Renew the full 600-second Phase 0 seven-scenario soak at current clean commit `ea234ea`.
  The authoritative schema-2 wrapper passed 26 complete matrices (182 scenario executions,
  962 declared work iterations) over 618.843 measured seconds. All 26 child report hashes are
  unique; start/end commits match; no tracked changes or invocation/report-read failures occurred.
  This renews headless Software decode/`h264_mf` evidence only; live audio, GUI, packaged playback,
  physical scanout, and cross-hardware gates remain open. See `docs/phase0-scenarios.md`.
- [x] Renew the full 600-second Phase 0 seven-scenario soak after adding true in-flight project
  switching. At clean commit `2b89378`, 41 complete matrices passed over 602.640 measured seconds:
  287 scenario executions and 1,681 declared work iterations with 41 unique child-report hashes,
  stable clean source provenance, and no invocation/report-read failures. Every switch run proved
  eight real in-flight cancellation suppressions, eight stale prior-generation rejections, eight
  fresh presentations, and zero final ownership/errors. This is headless Software/`h264_mf`
  evidence only; GUI, live audio, packaged playback, renderer GPU, scanout, and cross-hardware
  gates remain open. See `docs/phase0-scenarios.md`.
- [x] Renew the full 600-second Phase 0 seven-scenario soak after the shifted Matroska-duration and
  shared schema-9 validator checkpoints. At clean commit `d2b35a5`, 35 complete matrices passed
  over 610.336 measured seconds: 245 scenario executions and 1,435 declared work iterations with
  35 unique child-report hashes, stable clean source provenance, and no invocation/report-read
  failures. This remains headless evidence; GUI, live audio, packaged playback, renderer GPU,
  physical scanout, and cross-hardware gates remain open. See `docs/phase0-scenarios.md`.
- [x] Prefer completed monitor sessions during demand-driven capacity reclamation. Source groups
  remain whole and priority-protected; within equal visual priority the least-recently-requested
  completed group is reclaimed before the established active-work fallback. A real two-permit,
  three-source Phase 0 path proves exact deferred retry identity, preserves the newer resident, and
  reaches zero final ownership. The clean `b668543` requalification passed 32 complete matrices,
  224 scenario executions, and 1,312 declared work iterations over 617.912 seconds; report SHA-256
  is `80C965A68011CC0CEB950744176C803857E70CBF104054B99D15B07A3D871FE5`.
  This is not a general time-based idle reaper or cross-hardware proof. See
  `docs/phase0-scenarios.md`.
- [x] Add a clean-commit Phase 0 timeline-foundation runner around the existing release gates.
  The retained schema-1 run at `6576d91` passed ten independent 50,000-clip history trials
  (press/release p95 0.2672/0.7453 ms), wide/detail/playhead CPU p95
  0.4578/0.2900/0.2787 ms, and the real H.264 plus 20,002-bar gate at 0.4839 ms p95.
  The report omits the private media path and explicitly excludes GUI, scanout, package and
  cross-hardware claims. See `docs/performance-reports.md`.
- [x] Make proxy generation/deletion cancel and reset nonblocking for interface actions.
  Child supervision, bounded pipe/event queues and kill/wait ownership stay on workers;
  reset retains bounded cleanup slots and rejects overlapping cache mutation until finished.
  One before-failing regression now passes; 804 release tests, both real-media proxy tests,
  strict Clippy, formatting, fixtures and Phase 0 scenarios pass. Parent-reviewed.
- [x] Refresh the portable package from `9a17a2c`. Executable SHA-256 starts
  `F61C4CA8A840478B`; full previous package backed up and hash-verified. Runtime/static-import
  checks pass. No editor launch; GUI smoke remains `not_run`.
- [x] Move proxy completion, enable and cache reconciliation file checks to one owned,
  bounded worker. Checking keeps original media active; late replies cannot undo newer
  choices, relinks, deletion or reset. Full-cache checks drain in batches instead of being
  skipped. Two before-failing regressions now pass; 800 release tests, both real-media
  proxy tests, strict Clippy, formatting, fixtures and Phase 0 scenarios pass.
  Parent-reviewed (independent agent unavailable). See `docs/proxy-media.md`.
- [x] Refresh the portable package from `7c51591` with background activation/reconciliation.
  Executable SHA-256 starts `5A64D502EAC2F619`; complete previous package backed up and
  hash-verified. Runtime/static-import checks pass. No editor launch; GUI smoke is `not_run`.
- [x] Move `ProxyJob::start` source/tool validation and fingerprinting to its existing
  worker. Two before-failing regressions now pass; pre-cancelled state and source-change/
  cancellation contracts remain intact. 786 release tests, both real-media proxy tests,
  strict Clippy, formatting, fixture contracts and Phase 0 scenarios pass.
  Parent-reviewed; independent agent unavailable. See `docs/proxy-media.md`.
- [x] Rebuild the portable package from `ac52c2b` with worker-side proxy startup
  validation. Executable SHA-256 starts `D3FA11E3C8894DDA`; previous package backed
  up, runtime/static-import checks pass. No editor launch; GUI smoke remains `not_run`.
- [x] Resolve one complete FFmpeg/FFprobe pair on the startup-resources worker and route its
  canonical absolute FFmpeg path to Quick Export, Kraken Upscale and proxy generation. Adjacent
  package tools take priority, `FFMPEG_DIR/bin` is the developer fallback, pairs are never mixed,
  and ambient `PATH` lookup is forbidden. Two new regressions pass with 806 release tests, both
  real-media proxy tests, strict Clippy, formatting, seven fixture contracts and seven Phase 0
  scenarios. No editor launch. See `docs/runtime-media-tools.md`.
- [x] Refresh the portable package from `5e39177` with shared runtime-tool resolution. The complete
  previous 23-file package is archived and hash-verified; all 13 pinned runtime hashes, 15 AMD64
  static-import inventories, FFmpeg/FFprobe loader checks and the exact full-path launcher's
  check-only mode pass. Executable SHA-256 starts `C42458E86AE972F2`; only the executable and
  package status changed. No editor launch; GUI smoke remains `not_run`.
- [x] Rediscover matching local proxies after timeline placement/project reopen on the
  existing analysis worker; show ready but retain original-quality playback until explicit
  opt-in. Protect against stale projects, relinks and late replies after user actions.
  Six new regressions include real proxy generation/save/reopen. 783 release tests,
  strict Clippy, formatting, seven fixture contracts and updated Phase 0 scenarios pass.
  Parent-reviewed; independent review unavailable. See `docs/proxy-media.md`.
- [x] Update the portable package with cached-proxy rediscovery from `fb5c94d`.
  Executable SHA-256 starts `CDFC00DB47444A5B`; previous 23-file package archived and
  hash-verified. Runtime/static-import checks and launcher check-only mode pass.
  No editor launch; GUI qualification remains open (`smoke_status: not_run`).
- [x] Preserve exact rational export clocks for video, stills, titles, background
  and transition mattes. Both new regressions fail on rounded decimal clocks and
  pass on original fractions; seven live clock cases, 777 release tests, strict
  Clippy, formatting, seven fixture contracts and the updated Phase 0 runner pass.
  No resolution/audio/preview policy change. Parent-reviewed; independent agent
  unavailable. See `docs/exact-export-frame-rate.md`.
- [x] Rebuild the portable package with the exact export-clock fix from `ca99dfd`.
  New executable SHA-256 starts `41BD27272A4CFDE7`; the complete previous package
  is backed up and hash-verified. All 23 files are inventoried, pinned runtime and
  AMD64/static-import checks pass, and the launcher check-only mode passes.
  Only executable/status changed; no editor launch, `smoke_status: not_run`.
- [ ] Qualify the rebuilt package's GUI/export behavior and windowed performance
  through the exact launcher after explicit launch permission. Static package
  checks do not close this gate. See `docs/exact-export-frame-rate.md`.
- [x] Add 20 shifted ProRes/DNxHR VFR export checks: source heads, pretrimmed/slipped
  ranges, tail exclusion and final frames at 30 and 30000/1001 fps. All 88 output
  identities/counts/timestamps match; a nearest-frame mutation fails both tests.
  Production policy is unchanged. 775 release tests, strict Clippy, seven fixture
  contracts and the updated Phase 0 runner pass. Parent-reviewed; independent
  agent unavailable. See `docs/shifted-vfr-export-parity.md`.
- [x] Disable automatic Windows wake-priority boosts only on video monitor workers,
  preserving base/process/UI/audio priorities and full image quality. Same-binary
  control failed at 14,262 us; test-only no-boost runs passed at 89/59 us. Actual
  worker-policy regressions fail before/pass after; 773 release tests and strict
  Clippy/formatting pass. Parent-reviewed; independent agent unavailable.
  Evidence: `docs/monitor-worker-scheduling.md`.
- [x] Qualify production worker policy with uninstrumented four/eight-CPU Full
  audio runs: 30 seconds each, submit p95 55/51 us, zero audio/monitor faults.
  Four-CPU ten-minute resources pass: 56,464 matching frame requests/presentations,
  74 us submit p95, bounded cache/sessions, and zero post-drop sessions.
- [x] Finish eight-CPU sustained resources: 72,596 matching frame requests/presentations,
  66 us submit p95, zero drops/errors, bounded memory/sessions and clean teardown.
  Eight same-binary opposite-order readiness diagnostics also complete. Fixed-policy
  submission gains repeat; readiness differences reverse direction, so no consistent
  penalty or end-to-end speedup is established. All temporary hooks removed; fresh
  773 release tests, strict Clippy and formatting pass. See `docs/monitor-worker-scheduling.md`.
- [x] Rebuild the portable package with silent prewarming and worker wake scheduling,
  without launching it. Executable SHA-256 starts `5DD49EF46A5BEBD6`; all 23 previous
  package files are backed up and verified. Pinned runtime hashes, 15 AMD64 PE
  inventories, FFmpeg/FFprobe loader checks and launcher check-only mode pass.
  Only executable/status changed; `smoke_status` remains `not_run`.
  Evidence: `docs/monitor-worker-scheduling.md`.
- [ ] Complete independent review and requalify windowed/cross-hardware performance.
  Short paired results do not prove readiness non-inferiority or lag-free playback.
  The rebuilt package is prepared, not GUI-qualified; explicit launch approval is pending.
- [x] Make speculative monitor prewarming silent without removing decode/cache/session
  warming. Both standalone and coordinated workers retain visible foreground/reverse
  replies and errors. Five new regression tests, 770 release tests, strict Clippy,
  ten repeated prewarm checks, and independent review pass.
  Evidence: `docs/silent-monitor-prewarm.md`.
- [x] Rerun restricted-CPU resource gates after silent prewarming: both ten-minute
  Full-1080p runs pass (4 CPUs: 72,528 requests, 924 us submit p95; 8 CPUs: 86,620,
  98 us), with zero drops/errors and zero sessions after App drop. A 30-second
  four-CPU run still fails at 1,042 us; retain that variability evidence.
- [x] Repair the native-audio harness's full-duration video workload. All four
  timeline clips now cover measurement plus warmup and one second of slack, with
  exact four-source/Full
  assertions before every timed submission. Two new regressions, 772 release
  tests, strict Clippy, and formatting pass. Production code/limits are unchanged.
- [x] Requalify restricted-CPU native audio with the corrected full-duration workload.
  The corrected 30-second four-CPU run delivered 2,340 actual layer requests but
  failed at 16,148 us submission p95, one callback-lock failure, and 480 underrun
  frames. Two archived test-only traces locate most submission delay at
  `SourceLaneActor::submit`'s `wake.try_send`; controlled wake-boost evidence now
  supports the targeted worker policy above. Attribute audio lock ownership separately.
  No quality/threshold changes. Production four/eight-CPU runs now pass as recorded
  above; the historical failure remains preserved. The package now contains the
  fixes but has no new GUI qualification. Evidence: `docs/restricted-live-audio-submission.md` and
  `docs/monitor-worker-scheduling.md`.
- [x] Correct the Windows scaler CPU guard under restricted process affinity. Actual
  initialized scaler probes pass for 1/4/7/8/28 allowed processors; 765 release tests,
  strict Clippy, 304 hardware pixel/timestamp cases, and independent review pass.
  Eighteen before/after Full-1080p latency runs pass the scheduler gate; no speedup
  is demonstrated. Package rebuilt, runtime/hash checks pass; executable hash starts
  `03E01F2EB32BFA3B`. GUI smoke remains `not_run`; previous executable/status archived.
  Evidence and limits: `docs/cpu-budget-conversion.md`.
- [x] Qualify local restricted-CPU resource/native-audio gates after the CPU-count correction.
  Both native-audio runs and both four/eight-CPU ten-minute resource tests pass with
  the worker policy. The paired readiness follow-up is complete but timing is mixed.
  Keep Full resolution and filters; logical-CPU affinity is not a substitute
  for actual lower-core hardware or windowed playback qualification.
- [x] Pack full-resolution monitor RGBA directly into its final shared allocation, preserving
  exact pixels/alpha/letterboxing and cache ownership. Native 1080p packing p50 falls locally
  from 2.658 to 1.363 ms; native 4K from 11.659 to 5.820 ms. 1,296 small-layout comparisons,
  full-resolution/lifetime/bounds tests, 152 hardware pixel/timing cases, 764 release tests,
  strict Clippy, and independent review pass. Packing timing now includes final allocation.
  Three four-source Full-1080p latency runs pass at 63-67 ms frame-ready p95; this does not
  demonstrate end-to-end scrub improvement. Rebuilt package hash starts `4715E17526744CC8`;
  runtime/hash checks pass, GUI smoke remains `not_run`, no editor launch.
  Evidence: `docs/monitor-rgba-packing.md`.
- [x] Requalify ten-minute cache/session resources after single-allocation RGBA packing at
  source `e0f35d155c42af307581d216e6798127e4e8d43c`. 600.034 seconds, 21,072 four-source
  Full-1080p cycles / 84,288 requests, zero errors, 79 us scheduler p95, and 41 ms coarse
  matching-frame p95. Cache peak 215,654,400 bytes below 1 GiB; five peak sessions below eight
  and zero after App drop. Working-set growth 35,262,464 bytes below the diagnostic bound.
  Three rejected stale events are within the unchanged 85-event allowance. Package unchanged;
  no GUI launched. Evidence: `docs/phase1-sustained-soak.md`.
- [x] Requalify local ten-minute cache/session resources after accurate threaded conversion
  at source commit `b3e9228939f4b3edf2c2d98a74cf1be0d5338ba5`. 600.047 seconds,
  20,184 four-source Full-1080p cycles, 80,736 requests, 77 us scheduler p95,
  42 ms coarse frame-ready p95, zero monitor errors, and zero post-drop sessions.
  Cache peak 215,654,400 bytes under 1 GiB; five peak sessions under eight;
  sampled working-set growth 19,812,352 bytes under the diagnostic allowance.
  Raw samples/hashes verified; historical evidence preserved; no GUI launched.
  Evidence: `docs/phase1-sustained-soak.md`. Package remains unchanged.
- [x] Reduce accurate full-resolution conversion overhead without changing pixels, filters,
  or resolution. Bounded two-thread conversion cuts local bicubic scaler p50 from
  5.13-5.24 ms to 2.75-2.80 ms. 400 exact legacy comparisons, 152 Windows hardware
  pixel/timing cases, 753 release tests, strict Clippy, and independent review pass.
  Three paused Full-1080p latency runs: four-source frame-ready p95 63 ms;
  scheduler p95 153-274 us. Not a sustained/windowed or cross-machine claim.
  Portable package rebuilt, runtime/hash checks pass; SHA-256 starts `3A4D88D867281034`.
  GUI smoke remains `not_run`; no editor launch. Prior executable archived locally.
  Evidence: `docs/threaded-monitor-conversion.md`.
- [x] Repair missing-backend evidence in the paused/full-resolution latency gate without
  disabling prewarming or attributing cache hits to a decoder. Successful-work history is now
  bounded and independent of latest-wins frame events. Deterministic before/after regression,
  three 40-trial latency runs, 747 release tests, strict Clippy, 152 exact Windows hardware
  pixel/timing cases, and independent review pass. Four-source scheduler p95: 189-270 us;
  coarse matching-frame p95: 69-70 ms. This is not a windowed or sustained playback claim.
  Portable package rebuilt with runtime/hash checks; SHA-256 starts `0F30B9388BB8861B`.
  GUI smoke remains `not_run`; no editor launch. Previous executable archived locally.
  Evidence: `docs/phase1-latency-comparison.md`.
  Commit: `9df14ca58e21d79cf45321adb14e07e7f5ef7c28`.
- [x] Correct native-size planar/NV12 color conversion without reducing resolution.
  GPU-free regression reproduces the old failure; four explicit Windows hardware
  tests pass 152 exact H.264/HEVC Main 10 timing/pixel cases, including full 1080p.
  744 release tests, strict Clippy, seven fixture contracts/Phase 0 scenarios, and
  independent review pass. Portable package rebuilt without GUI launch, hash starts
  `8F0965B4489D34A11`; smoke remains `not_run`; runtime/hash/cleanup checks pass.
  Evidence and performance tradeoff: `docs/hardware-decode-parity.md`.
  Commit: `fb5335df898f69993f3ff5545260f0e47f157b1d`.
- [ ] Requalify windowed playback after accurate threaded conversion, direct RGBA packing,
  the Windows CPU-count correction, silent prewarming and worker wake scheduling.
  Prior local ten-minute resource qualification does not establish real-time display. Four
  Intel/NVIDIA one/four-source cases are prepared without launch; explicit permission to run
  the editor through the exact launcher is pending. Regenerate prepared package identities for
  the new executable before running. Qualify lower-core CPU contention before
  broad performance claims. The goal remains active; no automatic editor launch is authorized.
- [ ] Continue profiling RGBA packing, upload, and presentation if full-quality latency remains
  above the required gate. Never hide cost by lowering resolution or changing the selected filter.

- [x] Qualify reordered VFR MPEG-TS playback and source-time normalization.
  Commit: `21768e45740b58837c0f491ba6e0b5d4b8cdead2`.
- [x] Prove a deliberately delayed Full-1080p source cannot stall ready sources or the native audio
  playback clock. Commit: `cb90a9e5d396b902fb96cc8d6732e60deed851cf`.
- [x] Prove real-media layer toggling and backward scrubbing reject obsolete generations.
  Commit: `7dca05c1899e5c36f08a136ba801d18eb93a29fd`.
  Evidence: `docs/phase1-generation-stress.md` (32 cycles, four sources, 96 resource checkpoints).
- [x] Reduce relocation work to the crossed clip range and fix distant collision rejection,
  including linked A/V atomicity. Evidence: `docs/timeline-relocation-performance.md`.
  Commit: `e9c2bd4f413f58ef4e0279d6aeb3e6a373039b02`.
- [x] Pass the local 50,000-clip history budget with immutable shared clip records. All ten release
  trials passed both unchanged 2 ms limits: press p95 0.3481 ms, edit/release p95 0.9921 ms.
  Eight isolation/history/schema tests pass; dense drawing and cache evidence stay within budget.
  Evidence: `docs/timeline-relocation-performance.md`.
  Commit: `e4ba6f5fa54798a00b7cd0e588b7c264367f9008` (also includes decoder test teardown).
- [x] Make the reverse-scrub decoder test wait for asynchronous retirement before deleting
  its fixture; ten focused release trials pass without Windows sharing error 32.
- [x] Repair the intermittent two-second audio-crossfade export stall. Saved-command baseline
  failed 6/20 runs; finite sample padding/trimming plus clock reconstruction passes 20/20,
  as do twenty production crossfade tests. Corrected outputs match a successful original exactly.
  Fault-clock and empty-input regressions fail against the old chain and pass with the fix.
  FFmpeg diagnostics now retain a bounded 64 KiB tail; polling errors release child processes.
  Three parallel release workspace runs: 720 tests pass each. Strict Clippy and review pass.
  Evidence: `docs/audio-export-boundary.md`; original failure artifacts remain preserved.
  Commit: `b1d5acec07e3f6c44b054ccce73aec228ae0dc74`.
- [x] Qualify current cache/session limits in a ten-minute four-source Full-1080p stress run.
  600.042 seconds / 15,933 cycles; cache peak 215,654,400 bytes under 1 GiB; five peak sessions
  under eight and zero after app drop. Raw samples and provenance verified; no task processes left.
  Evidence: `docs/phase1-sustained-soak.md`. Local Software backend; broader UI/hardware gates remain.
  Commit: `98a4b72b00431507bd5d6eb53517f8f703b96a13`.
- [x] Prepare a current Windows package without launching the editor. The build-only path preserves
  historical smoke reports and binds explicit `not_run` smoke status to the new executable hash.
  Release build, launcher file check, packaged FFmpeg/FFprobe loader checks, runtime copy hashes,
  environment restoration, and cleanup pass. No GUI/performance qualification is claimed.
  Evidence: `docs/phase1-ui-qualification.md`.
  Commit: `1d0e9ef91eab0c4e787abe132d785a43b3bb2bb5`.
- [x] Implement the windowed four-source ruler-input probe, exact native upload/composition/blit
  correlation, and bounded approved-launcher runner. Preparation defaults to no GUI. Headless
  gesture/identity/timeout tests, 23 report rejection cases, Windows process ownership checks,
  728 release workspace tests, and strict Clippy pass. Live evidence remains pending.
  Evidence: `docs/phase1-ui-qualification.md`.
  Commit: `821c123230717a27b1e2a60732f74ddd6d886dd3`.
- [x] Run all four authorized one/four-source windowed cases on Intel UHD 770 and RTX 3090.
  Fixed the discovered rational timestamp-rounding mismatch: a correct decoded frame one microsecond
  before the requested boundary no longer leaves app completion and the probe pending. Shared decoder
  semantics reject two-microsecond preroll; all identity and CPU limits remain unchanged.
  All 48 inputs per case pass; input p95 0.3178–0.3596 ms; frame CPU p95 0.8291–1.3925 ms.
  730 release tests pass (16 opt-in ignored), strict Clippy and independent review pass. Exact evidence
  hashes and process cleanup verified; all four editor instances closed. User authorized these
  qualification launches and corrective reruns through the approved launcher.
  Evidence: `docs/phase1-ui-qualification.md`; live run `windowed-40b45f22-6f88-4afd-a9bf-b6af537ef072`.
  Commit: `762fbf88fb9edd2066fa4ba1ca8f0c379d429046`.
- [x] Reduce repeated scrub preroll with nearest-keyframe seeks, one conservative fallback, and
  250 ms nearby scrub reuse; preserve five-second non-scrub reuse. Baseline four-source p95
  523.0007/455.5999 ms becomes 86.1059–104.0912/73.8769–91.8742 ms on Intel/NVIDIA in two clean
  final-package repetitions. CPU limits, pixels, cache/session caps, and identity checks pass.
  Also fix repeated EOF draining so the last delayed B-frame remains available across requests.
  Real-media pixel/reference regressions cover MPEG-4, reordered MPEG-2 TS, and supplied open-GOP
  H.264; original packet-work and final-frame failures reproduce before the fixes. 735 release
  tests, strict Clippy, independent review, evidence hashes, package identity, and cleanup pass.
  Evidence: `docs/scrub-seek-performance.md`. Final package SHA-256 starts `69F330ADC16BA969`.
  Commit: `8312ad10cc5ea1e09c8427cc36c51beda76944fd`.
- [x] Reproduce incoming pointer movement changing the probe's held ruler drag through actual egui.
  Preserve a failed pending sample even when an eligible paint arrives on the same frame; the
  regression fails against the original presentation guard and passes with the correction.
  Add bounded, payload-free backend input summaries without suppressing input or relaxing guards.
  One new four-case windowed run passes; 737 release tests, strict Clippy, validator checks, and
  independent review pass. Evidence/package hashes and process cleanup verified.
  Evidence: `docs/windowed-input-integrity.md`; current package SHA-256 starts `BCCA5054FC296694`.
  Commit: `f1b25f3836d58f00faf7c7760e2228475da805d5`.
- [ ] Attribute any future workload-integrity interruption using the new input diagnostics.
  Historical attempts `5304fe46` and `91bd3123` lack input evidence; their specific origin remains
  unproven. Both failed reports and the earlier two clean runs remain intact. Unattended input
  isolation is not implemented. Do not silently retry until green or count interrupted timing.
- [x] Fix DNxHR preview using BT.601 despite decoded frames declaring BT.709. Configure each frame's
  YUV matrix/range before conversion, preserving untagged defaults, YUVJ full range, and RGB/alpha.
  Original pixels exactly match forced BT.601; the old path fails the independent pixel regression,
  and the fix passes. ProRes/DNxHR 10-bit shifted-VFR MOV and supplied HEVC Main 10 pass 57 exact
  forward/reverse/fresh/final-frame checks; app analysis/preview preserves both MOV local indexes.
  743 release tests, strict Clippy, seven deterministic fixture contracts, seven Phase 0 scenarios,
  and independent review pass. Evidence: `docs/codec-color-qualification.md`.
  Rebuilt package SHA-256 starts `F44F08C87FDFD151`; general smoke remains `not_run` (no GUI launch).
  Evidence/package identities and process cleanup verified; prior windowed package archived.
  Commit: `b389c58860fa64a84a48f9c52a647307b15437b2`.
- [ ] Continue broader codec/backend/reference-machine and sustained playback/audio qualification,
  preserving separate input CPU and matching-frame measurements. Recent 74–135 ms local
  fresh-frame p95 is not physical input/scanout latency or full real-time playback qualification.

## Phase 0–1 verification queue

- [x] Add the local-only shifted AOM AV1 WebM sibling without weakening the existing Matroska
  contract. FFmpeg 8.1 stream-copies the validated temporary Matroska artifact with `-copyts` and
  explicit WebM muxing; separate output hashes/sizes and all eight shifted packet timestamps are
  pinned before either artifact is published. The Phase 0 runner routes and restores
  `MAELSTROM_AV1_WEBM_VFR_TEST_MEDIA`; waveform, preview, save/reopen, and export source-identity
  tests prove matching local timing/identity across both containers. This is local container coverage,
  not broad WebM/AV1 or hardware qualification.

- [x] Route the packaged playback-soak harness exclusively through
      `H:\Maelstrom Rust\Launch-Maelstrom-Editor.bat`. Its non-launching validation proves the
      derived packaged runtime identity without modifying retained soak evidence, and the contract
      rejects direct executable paths.
- [x] Remove the remaining direct GUI launches from cross-adapter qualification and optional
      package smoke. Both now bind the exact packaged editor beneath the canonical waiting launcher,
      clean only that fresh process tree, and retain non-launching contract coverage.
- [x] Add the opt-in packaged disruption harness for eight scrub/eight restore, offline/recovery,
      Full-quality cache-pressure, and cancelled-export paths. It uses only the exact launcher with
      `--cache-mb=512`; its dry-run contract proves no GUI, FFmpeg, environment, or artifact side
      effect. Success publication is cleanup-gated for the captured launcher tree, restored
      environment, and owned media/export artifacts; live/cross-hardware evidence remains open.
- [ ] Complete broader cross-hardware ten-minute playback and seven-scenario soak evidence.
- [ ] Complete UI-present/cross-hardware four-source latency evidence.
      Local one/four-source CPU and exact native-layer presentation cases pass on both adapters;
      broader machines and fresh-frame latency remain open. See `docs/phase1-ui-qualification.md`.
- [x] Add real-media stress coverage for rapid layer enable/disable plus backward scrubbing; accept
      only the latest generation.
- [x] Confirm cache and decoder-session memory remain inside configured hard limits for ten minutes.
  Qualified locally through exact cache/session peaks; whole-app RAM and actor accounting are separate.
- [ ] Expand VFR qualification across additional codecs, reorder patterns, containers, and decoder
  backends.
  Generated FFV1 level-3 Matroska now adds a software-only eight-frame 320x180 yuv420p
  VFR case with a nine-second source origin, no audio, and exact fixture identity. Its
  local presentation boundaries pass CLI-reference scrub, app floor/hold routing, and
  production-graph source-identity tests. A real save/reopen regression proves the runtime-only
  frame index is omitted from project JSON and reconstructed by asynchronous media analysis with
  identical exact-boundary, hold, and timeline-end addressing. Import reconciles the source, placed clip, and
  video strip to 542 ms while retaining the 9.542-second raw container duration for
  inspection. This is one intra-only lossless/container
  checkpoint; the 14-fixture Phase 0 matrix and 910-test serial release workspace pass,
  while broad VFR qualification remains open.
  Generated FFVHUFF Matroska now adds a clean software-only 160×90 lossless
  RGB-family/BGRA VFR path with eight selected frames, a five-second origin, and
  no audio. Exact CLI scrub, app floor/hold/local-duration, and production export
  source-identity checks pass; all eight FFprobe keyframe/I-picture labels are
  pinned. BGRA is alpha-capable, but alpha preservation is not claimed. This is
  one codec/container checkpoint; broad VFR qualification remains open.
  Generated shifted/reordered MPEG-4 MP4 now adds one deterministic case combining irregular VFR,
  a three-second stream origin, B-frame packet reordering, and exact waveform/decode/app local-time
  checks. Generated ProRes/DNxHR 10-bit MOV and supplied HEVC Main 10 also have exact local Software
  timing/pixel evidence. ProRes/DNxHR and shifted/reordered MPEG-4 now also pass 30
  export-graph source-identity cases with a test MPEG-4 encoder. Named CUVID/QSV H.264, HEVC, and
  AV1 now join D3D11VA/DXVA2 in the exact local hardware matrix; shifted AV1 analysis/app routing is
  also covered. Broader AV1/camera sources, QSV reverse performance, cross-machine hardware/color
  parity, and production-encoder conformance remain open.

## Phase 2 implementation queue

- [x] Add bounded compositor-owned render-target/texture reuse and retained CPU command scratch;
  `egui_wgpu` owns the callback encoder, so the compositor allocates no command buffers to pool.
- [x] Implement correct premultiplied-alpha image/video semantics.
- [x] Add nearest, bilinear, and bicubic preview sampling where supported.
- [x] Prove four transformed 1080p layers on the discrete reference profile with the schema-3
  headless compositor qualification. Presentation and physical scanout are separate gates.
- [x] Prove at least two layers with Auto quality on the integrated reference profile. The
  schema-1 headless app gate binds two real 1920x1080 sources to production Auto hysteresis,
  fresh Full-to-Half resubmissions, and two exact Intel UHD 770 compositor probes. It does not
  claim organic decoder pressure or window/scanout presentation.
- [x] Prove missing or late layers never stall ready layers or input. Schema-3 GPU scenarios cover
  missing/late texture output; renewed real-media missing-upper and supersession stress cover the
  independent monitor scheduler. This remains headless evidence, not physical presentation.

## Professional editor backlog

- [x] Complete the schema-v1 graph/runtime foundation before expanding the effect catalog. Durable
  migration, typed IDs/ports/parameters, bounded validation, distinct immutable compiled plans,
  prederived curve data, four-slot latest-wins worker compilation, generation/source rejection,
  bounded runtime caching, owner-thread whole-plan swaps, and export-plan compilation are covered by
  focused parity/lifecycle tests. Output-size/color-setting invalidation and renderer resource
  caches remain with their later roadmap items.
- [x] Close the Phase 3 ten-node capacity, live-edit, stale-result, and exact-state gates. Timeline,
  UI authoring, renderer/WGSL buffers, curve LUTs, and export now agree on ten nodes; the app keeps
  playback active through 120 rapid edits under the 8 ms UI p95 budget and installs only current
  compiled work. Current-format save/reopen and UI undo/redo preserve the exact full graph and keys.
- [x] Close the Phase 3 encoded-RGBA effect parity gate. A real RTX 3090/Vulkan native render and
  the production FFmpeg graph lowering evaluate the same animated Brightness/Contrast, master/RGB
  curves, and Vignette stack with full-frame maximum error 0 (tolerance 4); the neutral boundary is
  1. Export now avoids zero-degree rotation loss and forces RGB video/matte/title overlays instead
  of FFmpeg's hidden 4:2:0 default. Evidence: `docs/phase3-effect-parity.md`.
- [ ] Complete preview/export parity for effects, transitions, titles, and color processing.
- [x] Add the remaining roadmap transition families: Dip to White, four-direction Wipe and Slide,
  plus true four-direction Push that moves both sources. Preserve existing Slide project behavior;
  provide nested EN/JA Effects/context menus, drag/drop, undo, all-kind persistence, and matching
  preview/export motion. Real FFmpeg midpoint samples cover every Push direction, including a
  mixed incoming Slide/outgoing Push regression. See `docs/video-transitions.md`.
- [ ] Add the remaining title tooling, Rec.709 color pipeline, LUT validation, and non-blocking
  scopes.
- [ ] Continue the professional audio engine: buses, routing, automation, meters, callback-safe DSP,
  channel layouts, shuttle audio, and loudness analysis.
- [ ] Add the Phase 5B offline transcription, captions, and text-based editing workstream described
  in `docs/transcription-and-text-editing.md`.
  - Start with a versioned bounded sidecar/schema, a sidecar-owned metadata registry for large ASR
    assets, an empty-model fake backend, and English/Japanese accuracy/timing fixtures so public
    checkout/build/test stays model-free and fully functional. Do not use the eager in-process
    preloader for multi-gigabyte speech weights.
  - Use the locally proven `large-v3-turbo`/faster-whisper/Silero stack as the baseline, then
    benchmark Qwen3-ASR plus its Japanese-capable aligner on identical PCM before selecting the
    default. Keep WhisperX, diarization, and language-limited fast engines optional and disclosed.
  - Add a detachable Text workspace with Transcript/Captions/Graphics, search, speaker/confidence
    filters, follow/jump, corrections, caption-track generation, and SRT/WebVTT interchange.
  - Make filler/repetition/pause removal a reviewed, non-destructive, single-undo ripple edit with
    linked-A/V validation and short adjustable audio crossfades; reject locked, transition-occupied,
    or handle-starved candidates explicitly and never silently delete words.
  - Gate release on EN WER, JA CER, timestamp/diarization accuracy, long-form drift, cancellation,
    stale-result rejection, derived-cache invalidation, caption readability/CJK segmentation,
    enforced offline inference with no implicit downloads, and zero playback-quality or UI regression.
- [ ] Add multiple sequences, nesting, speed/remap, relink/consolidate, and project interchange in
  roadmap order.
- [ ] Evaluate optional GPU Optical Flow interpolation (`fruc_vulkan`) for slow motion and
  frame-rate conversion; preserve the working FFmpeg runtime during isolated qualification.
  - The approved local `n8.1-maelstrom-20260824` runtime currently exposes `fps`, `framerate`, and
    `minterpolate`, but not `fruc_vulkan`. Treat upstream FFmpeg master as a separately qualified
    candidate; do not replace or destabilize the working 8.1 runtime merely because the GPU is
    nominally compatible.
  - Automatically detect usable devices, driver capabilities, and filter availability in the
    actual runtime. Do not enable by GPU model-name matching alone. Probe Vulkan optical-flow
    features, queues, supported formats/dimensions, and successful filter/session initialization.
  - On multi-GPU systems, select a compatible processing device independently of the display
    adapter, subject to a user's explicit device override. Keep probing bounded and off the UI thread.
  - [x] Add the first capability foundation: the startup-resources worker inventories the exact
    resolved bundled runtime, counts all Vulkan physical devices independently of the display
    adapter without collapsing duplicate models, then runs indexed real `fruc_vulkan` session
    probes inside one bounded FFmpeg budget. The Playback menu exposes session-only availability
    and the exact failure reason without changing quality or selecting interpolation. The approved
    FFmpeg 8.1 bundle correctly reports the filter as missing. See `docs/runtime-media-tools.md`.
  - Enable the Optical Flow option only when usable; otherwise keep it visible but disabled with
    a specific reason (runtime filter missing, unsupported GPU/driver, or unsupported clip format).
    Recheck when the device/runtime changes; do not cache support indefinitely after device loss.
  - Availability is automatic; applying interpolation remains the user's choice. Preserve saved
    project settings when unavailable, warn visibly, and require an explicit alternative before
    exporting with a different interpolation method. Never silently lower playback resolution.
  - Test supported/unsupported and mixed-GPU systems, missing filters, initialization failure,
    device loss, and preview/export parity before marking the feature implemented.
  References: [FFmpeg filter](https://ffmpeg.org/ffmpeg-filters.html#fruc_005fvulkan),
  [Vulkan capability contract](https://docs.vulkan.org/refpages/latest/refpages/source/VK_NV_optical_flow.html).
- [ ] Keep the compact EN/JA interface, default panel sizing, border padding, nested menus, and
  draggable/scrubbable numeric controls consistent as features land.
- [x] Add real detachable/re-dockable native frames for Media Pool, Viewer, Timeline, every
  Inspector/Audio/Color/Effects/Media section, and Undertow. Support cross-monitor movement, saved workspace placement,
  close-to-reattach, safe recovery when a monitor disappears, and one shared editor/undo/render
  authority. Keep the current organized panel sizing as the single-window default and prove that
  detaching does not duplicate decode work or break interaction budgets.
  - [x] Ship the native Edit panels for Media Pool, Viewer, Timeline, and each right-sidebar
    section, including four dock regions, tab groups, cross-window transition drops, local monitor
    geometry, mixed-DPI resize handling, and close-to-reattach behavior.
  - [x] Preserve the compact five-tab right sidebar as the organized default while allowing each
    tab to detach independently. Migrate the legacy combined Tools dock/detach and machine-local
    geometry without overwriting newer per-panel placements.
  - [x] Extend the native host contract to independent Undertow Tools and Mixer frames while
    reusing the one authoritative Timeline. Edit/Undertow dock positions and active tabs remain
    isolated, legacy projects migrate safely, and default-minus-detached layouts retain the
    organized center-weighted audio workspace.
  - [ ] Add saved named workspace presets without making machine-specific monitor geometry part of
    portable project state.

## Release and repository

- [ ] Keep optional/proprietary models and runtimes ignored while documenting source, license, and
  code integration.
- [ ] Keep the portable Windows package runtime-complete and launch only through
  `H:\Maelstrom Rust\Launch-Maelstrom-Editor.bat`.
- [ ] Complete license/redistribution audit for every packaged dependency.
- [ ] Produce a signed installer only after all portable-package gates pass.

## Maintenance rule

After each completed slice:

1. Update the matching authoritative roadmap evidence.
2. Check or revise the corresponding item here.
3. Record the commit hash under **Current stopping point** when it materially changes the next task.
4. Preserve local `.verify-*`, dependency, model, artifact, and package exclusions.
