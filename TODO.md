# Maelstrom TODO

Short operational queue for active development. `ENGINE_ROADMAP.md` is the authoritative feature,
architecture, performance, and exit-gate specification. `MAELSTROM_ACTION_PLAN_OVERLAY.md` provides
supplemental sequencing and constraints. If this list conflicts with either document, follow the
roadmap and correct this file.

Last updated: 2026-08-31

## Current stopping point

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
  Generated ProRes/DNxHR 10-bit MOV and supplied HEVC Main 10 now have exact local Software
  timing/pixel evidence. ProRes/DNxHR now also pass 20 export-graph source-identity
  cases with a test MPEG-4 encoder. AV1, broader camera sources, hardware/color
  parity and production-encoder conformance remain open.

## Phase 2 implementation queue

- [ ] Add compositor-owned render-target, texture, and command-scratch pools.
- [ ] Implement correct premultiplied-alpha image/video semantics.
- [ ] Add nearest, bilinear, and bicubic preview sampling where supported.
- [ ] Prove four transformed 1080p layers on the discrete reference profile.
- [ ] Prove at least two layers with Auto quality on the integrated reference profile.
- [ ] Prove missing or late layers never stall ready layers or input.

## Professional editor backlog

- [ ] Finish the schema-versioned effect graph before expanding the effect catalog.
- [ ] Complete preview/export parity for effects, transitions, titles, and color processing.
- [ ] Add the remaining roadmap transitions, title tooling, Rec.709 color pipeline, LUT validation,
  and non-blocking scopes.
- [ ] Continue the professional audio engine: buses, routing, automation, meters, callback-safe DSP,
  channel layouts, shuttle audio, and loudness analysis.
- [ ] Add multiple sequences, nesting, speed/remap, relink/consolidate, and project interchange in
  roadmap order.
- [ ] Evaluate optional GPU Optical Flow interpolation (`fruc_vulkan`) for slow motion and
  frame-rate conversion; preserve the working FFmpeg runtime during isolated qualification.
  - Automatically detect usable devices, driver capabilities, and filter availability in the
    actual runtime. Do not enable by GPU model-name matching alone. Probe Vulkan optical-flow
    features, queues, supported formats/dimensions, and successful filter/session initialization.
  - On multi-GPU systems, select a compatible processing device independently of the display
    adapter, subject to a user's explicit device override. Keep probing bounded and off the UI thread.
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
