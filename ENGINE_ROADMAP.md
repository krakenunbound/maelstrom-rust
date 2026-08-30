# Maelstrom Engine Roadmap

Status: planned after the completed Blick-class Windows foundation  
Target: Windows 10/11 x86-64  
Optional later target: Linux  
Out of scope: macOS

## Purpose

The foundation proves that Maelstrom can keep timeline interaction independent from media cost.
This roadmap turns that foundation into a professional editing engine without sacrificing that
property.

The objective is not to reproduce Blick's private implementation. The objective is to match or
exceed its observable strengths:

- timeline input never waits for decode, effects, compositing, audio, disk, or export;
- media results arrive asynchronously and stale work is discarded;
- playback begins without a mandatory index or render pass;
- source resolution and layer count may reduce preview quality, but never timeline responsiveness;
- caches remain bounded, generation-aware, and disposable;
- the inspector tells the truth about active hardware and fallbacks.

## Current baseline

The completed foundation already supplies:

- native Rust, `winit`, `wgpu`, and immediate-mode timeline rendering;
- O(visible) timeline drawing, banding, and sub-2 ms 50,000-clip interaction gates;
- sticky, cancellable, coalesced FFmpeg monitor decoding with Windows hardware fallback;
- live CPAL playback, waveform peaks, gain, mute, fades, and meters;
- core pointer/range/trim/roll/slip/razor operations and inverse undo history;
- autosave, recovery, project thumbnails, relative media paths, and project packaging;
- snapshot H.264/AAC export;
- bilingual English/Japanese UI.

The existing budgets and gates in `RUST_NLE_FOUNDATION_PLAN.md`, `ARCHITECTURE.md`, and
`FOUNDATION_AUDIT.md` remain mandatory throughout this roadmap.

## Non-negotiable invariants

- [ ] Timeline input-to-visual p95 remains at or below 1 ms normally and 2 ms with 50,000 clips.
- [ ] Full CPU UI frame remains below 8 ms p95 at 1920×1080.
- [ ] No filesystem, FFmpeg, effect evaluation, graph compilation, or blocking worker lock enters
      the UI/timeline path.
- [ ] Timeline drawing remains O(visible clips), not O(total clips or effect count).
- [ ] Playback clock never waits for video. Late video holds or skips; late audio emits silence and
      catches up without blocking.
- [ ] Every asynchronous result carries project, sequence, source, and generation identity.
- [ ] A result for an obsolete generation is discarded before it can update the viewer or cache.
- [ ] Frame, waveform, thumbnail, proxy, index, and effect caches have explicit byte or item caps.
- [ ] Project edits remain authoritative. Derived media is never mistaken for project state.
- [ ] Hardware acceleration always retains a tested software fallback.
- [ ] Export uses an immutable snapshot and never pauses editing.
- [ ] All FFmpeg distribution remains pinned, shared, LGPL-compatible, and license-audited.
- [ ] Every test run terminates its Maelstrom and FFmpeg process tree before returning control.

## Performance reference matrix

Before expanding capability, capture reproducible baselines on at least these Windows profiles:

| Profile | Minimum proof |
|---|---|
| Integrated Intel GPU with Quick Sync | Smooth one-layer 1080p30 full-resolution playback; smooth 4K30 at an automatically selected preview scale; timeline budgets unchanged. |
| Mid-range discrete NVIDIA or AMD GPU | At least four 1080p30 layers or two 4K30 layers using adaptive preview; no timeline hitch. |
| Current development machine | Preserve all existing 50k, 20k-plus-real-media, reverse-scrub, audio, and package results. |
| Software-decode mode | Correct preview and export on supported media; degraded picture cadence is allowed, timeline latency is not. |

Fixture coverage must include H.264 long-GOP, H.265/HEVC where legally available for decode, AV1,
ProRes or DNx-family mezzanine media, variable-frame-rate phone footage, 23.976/24/25/29.97/30/50/60
fps, mono/stereo/multichannel audio, still images, missing media, corrupt media, and 4K/8K sources.

## Execution order

Each phase has a stop gate. Do not begin dependent phases until the prior gate passes. Independent
audio work may proceed beside compositor work only when it does not touch the same project schema
or playback ownership.

### Phase 0 — Measurement and media corpus

Build the evidence harness before adding engine complexity.

- [x] Add a versioned local media-fixture manifest with checksum, codec, rate, dimensions, channel
      layout, duration, GOP pattern, and expected failure behavior. See `docs/media-fixtures.md`.
- [x] Add generated fixtures for tests that cannot depend on redistributable media.
- [x] Add an opt-in real-media corpus runner for large local files.
- [x] Record renderer GPU, decoder backend, encoder backend, driver, CPU, RAM, preview scale, cache
      cap, and display refresh in every performance report.
- [ ] Add ten-minute playback soak, repeated reverse scrub, rapid project switching, media-offline,
      memory-pressure, and export-cancellation scenarios.
      A finite prerequisite matrix covers the listed behavioral scenarios and includes a bounded
      350 MiB cumulative /
      280 MiB live runtime strip-cache checkpoint with per-insertion cap and exact oldest-eviction
      assertions; broader sustained pressure remains open. The exact packaged executable passed
      the 600-second Full/Full real A/V loop soak on 2026-08-28 with 10 loops, zero monitor errors,
      zero fallback uploads, zero audio underruns/lock failures/late discards, and 237,711,360 bytes
      peak GUI working-set growth. Its schema-2 resource evidence reported 268,015,616 peak decoded
      frame-cache bytes below the 1 GiB cap and one peak sticky session below the 16-session app
      cap. This combined item remains open for broader multi-source memory-pressure coverage. See
      `docs/phase0-scenarios.md` and `docs/performance-reports.md`.
- [ ] Add per-stage timing for demux, decode, transfer, scale, composite, upload, audio mix, and
      presentation submission without logging per frame in normal builds.
      Decoder-worker aggregates now cover cache lookup, demux, decoder calls, hardware transfer,
      scale, RGBA packing, and the active worker request. Viewer upload, changed-composition encode,
      presentation-call CPU boundaries, and audio output-callback timing are also reported. This
      item remains open for GPU completion/scanout.
- [x] Add visible dropped/held/late-frame counters and audio underrun counters to diagnostics.

Exit gate:

- [ ] The same report can be produced on integrated and discrete Windows hardware.
- [ ] Existing foundation gates remain green.
- [ ] A failing codec, driver, or stage is identifiable from one report without guessing.

### Phase 1 — Multi-source playback and adaptive preview

Turn the single topmost-source monitor into a scheduler capable of feeding a compositor.

- [x] Define immutable `PreviewRequest` data containing sequence generation, playhead tick, output
      size, preview quality, and ordered visible layer/audio-source descriptions. The app now
      captures ordered audible-source metadata in a fixed 64-entry request snapshot, with explicit
      overflow tracking and no cap on actual audio playback.
- [ ] Maintain one sticky decoder session per active source within a bounded session pool. A
      shared hard pool now limits all visible monitor decoders to four foreground and four
      speculative-background permits, with exact coherent playback-soak diagnostics and RAII
      release on every exit path. Deduplicating speculative contexts by active source remains.
- [ ] Prioritize visible/top layers and audible lanes; cancel sources no longer contributing.
- [x] Add per-source decoded-frame slots so one slow source cannot block other sources.
- [x] Add adaptive full/half/quarter/eighth preview resolution based on measured frame budget.
- [x] Add manual preview-quality override and an honest Auto mode.
- [ ] Add background proxy generation as optional derived media, never a prerequisite to edit/play.
- [ ] Preserve exact source-time mapping for VFR, rational project rates, trims, slips, and reverse
      seeks.
- [ ] Add decode-session eviction that respects the global byte/session cap.
- [ ] Expose active source backend, preview scale, proxy/original choice, and fallback reason.

Exit gate:

- [ ] Four independent 1080p sources can be requested concurrently without timeline latency
      regression. A preliminary opt-in local gate now creates four dynamic independent 1080p30
      MPEG-4 sources, submits one explicit Full-output request in under 20 ms, requires all four
      source frames within five seconds, and proves the exact shared 4 foreground + 3 speculative
      background / 8-cap session state with full post-drop release. It is not a timeline-latency
      regression baseline, p95, sustained, or cross-hardware proof. A second opt-in local gate
      now compares 20 isolated one-source and four-source Full-1080p trials, records nearest-rank
      p50/p95/max scheduler and matching-frame timings, and enforces only a 1 ms headless
      input-to-submit scheduler p95. It remains local Software-backend evidence rather than sustained, UI-present,
      or cross-hardware completion. See `docs/phase1-multisource.md` and
      `docs/phase1-latency-comparison.md`. A third opt-in headless local gate now repeats the
      same four-source Full-1080p five-second-fixture forward/back scrub workload for a bounded
      duration (600 seconds by default), with raw scheduler/frame-ready samples, exact runtime
      counter deltas, bounded cache/session evidence, post-drop release, and tracked-process
      working-set samples. It is not realtime playback, audio, visible UI, GPU compositor, or
      cross-hardware proof; see `docs/phase1-sustained-soak.md`.
- [ ] A deliberately slow source cannot delay a ready source or the playback clock.
      A deterministic test-only decoder barrier proves an independently scheduled ready source can
      complete while another decoder worker is blocked; this is not yet a real-media latency or
      playback-clock measurement.
- [ ] Rapid layer enable/disable and backward scrubbing publish only the latest generation.
      A deterministic app sequence covers forward/backward scrub, disable, re-enable, and newest
      re-enabled generation/request presentation while an unaffected layer remains retained;
      real-media stress coverage
      remains required.
- [ ] Cache/session memory remains inside its configured hard limit during a ten-minute stress run.

### Phase 2 — Real-time GPU compositor

Replace “topmost clip wins” with a generation-aware retained compositor.

- [ ] Add a compositor-owned render target pool, texture pool, and command buffer scratch storage.
- [x] Upload or import one latest-ready texture per contributing source without per-clip texture
      creation.
- [x] Composite ordered video layers with transparent empty regions over project background black.
- [x] Implement position, scale, rotation, anchor point, crop, opacity, and horizontal/vertical flip.
- [x] Implement project-size fitting modes: fit, fill, stretch, and original pixels.
- [ ] Implement premultiplied-alpha handling and correct image/video alpha semantics.
- [x] Add still-image layers with bounded texture downscaling.
- [ ] Add nearest/bilinear/bicubic preview sampling options where supported.
- [x] Double-buffer viewer outputs so graph execution never blocks timeline drawing.
- [x] Reuse compiled pipelines and bind groups; no shader/pipeline compilation during playback.
- [ ] Report composite time, active layer count, and selected preview scale.

Exit gate:

- [ ] At least four transformed 1080p layers composite correctly on the discrete reference profile.
- [ ] At least two layers operate on the integrated reference profile using Auto preview quality.
- [ ] Disabling a layer or changing a transform appears on the next available preview generation.
- [ ] Missing/late layers hold or become transparent without stalling ready layers or input.
- [x] Timeline and full-frame CPU budgets remain green.

### Phase 3 — Effect graph and parameter engine

Add a small deterministic graph before adding a large effects catalog.

- [ ] Define stable effect/node IDs, typed ports, typed parameters, and schema-versioned
      serialization.
- [ ] Separate immutable graph description from compiled runtime graph and derived caches.
- [ ] Validate cycles, missing nodes, incompatible ports, and unsupported versions without crashing.
- [ ] Compile graph changes on a worker; atomically swap the latest valid compiled graph.
- [x] Add bounded source-time brightness/contrast keyframes with linear, smooth, ease-in,
      ease-out, and hold interpolation shared by preview and export.
- [x] Expose the active correction's brightness/contrast keys as selected-clip timeline lanes with
      direct seek, frame-snapped drag retiming, collision protection, and one-step undo/redo.
- [ ] Add parameter keyframes with rational tick time, interpolation type, and bounded evaluation.
- [ ] Implement constant, linear, smooth/Bezier, hold, and ease interpolation.
- [ ] Add GPU kernels for transform, crop, opacity, blend, blur, sharpen, brightness/contrast,
      saturation, hue, and simple mask.
- [ ] Add CPU fallback only for effects that explicitly support it; never execute it on the UI
      thread.
- [ ] Add effect bypass, per-node enable, before/after viewer, and reset controls.
- [ ] Add graph/result invalidation by source generation, parameter generation, output size, and
      color settings.
- [ ] Bound intermediate render targets and reuse them through a lifetime-aware pool.

Exit gate:

- [ ] A ten-node graph can be edited while playing without timeline regression.
- [ ] Obsolete graph compilation and effect results never reach the viewer.
- [ ] Save/reopen and undo/redo restore exact graph and keyframe state.
- [ ] Export and preview evaluate the same graph description with matching frame results within
      defined color/rounding tolerance.

### Phase 4 — Transitions, titles, and color pipeline

Build these on the compositor/effect graph rather than as timeline exceptions.

- [x] Represent the first video transition as a bounded operation between exact adjacent clips,
      preserving the non-overlapping timeline invariant and explicit source-handle validation.
- [x] Add native cross dissolve with matching continuously advancing preview and export sources.
- [x] Add native dip to black without source-handle requirements, with matching preview/export
      half-window opacity and true black at the cut.
- [ ] Add dip to white, wipe, and push.
- [x] Add native equal-power audio crossfades with source-handle validation, independently editable
      start/end edges, sample-accurate live gains, and matching export envelopes.
- [x] Make cross-dissolve duration and curve directly editable and independently undoable.
- [x] Add the first durable native title overlay: text, size, alignment, fill, outline, shadow,
      normalized position, opacity, linear fades, direct timeline timing, undo/redo, and v4 project
      migration.
- [x] Use one bundled-font CPU raster contract for deterministic viewer preview and FFmpeg export,
      including English and Japanese text without machine-local font dependencies.
- [ ] Add a title media type with text, font, size, alignment, color, outline, shadow, background,
      and transform.
- [ ] Shape and cache glyph runs off the playback-critical path; support English and Japanese.
- [ ] Add safe-title/action guides and project-resolution-aware title layout.
- [ ] Define a linear working-space pipeline with explicit source interpretation and output
      transform.
- [ ] Add exposure, white balance, lift/gamma/gain, contrast/pivot, saturation, and curves.
- [ ] Add 1D/3D LUT import with validation and bounded GPU resources.
- [ ] Preserve an SDR Rec.709 default before attempting HDR.
- [ ] Add scopes on background/GPU analysis: waveform, vectorscope, RGB parade, histogram.

Exit gate:

- [ ] Transitions and titles play and export consistently at project rate.
- [ ] Japanese title shaping is correct and survives save/reopen.
- [ ] Color controls and LUTs are deterministic between preview and export within tolerance.
- [ ] Scopes may lag but never stall transport or timeline interaction.

### Phase 5 — Professional audio engine

Evolve the current lane mixer into a graph scheduled by the native audio clock.

- [ ] Add track, submix/bus, and master channel strips.
- [ ] Add volume, pan/balance, mute, solo, record-arm placeholder, and channel routing.
- [ ] Add sample-accurate clip and track automation with visible envelopes.
- [x] Add equal-power crossfades.
- [ ] Add adjustable audio fade curves.
- [ ] Add channel-layout handling for mono, stereo, 5.1, and source downmix/upmix policies.
- [ ] Add per-track and master peak, true-peak, RMS, and LUFS metering.
- [ ] Add built-in gain, polarity, channel mapper, high/low-pass, parametric EQ, compressor,
      limiter, gate, and delay.
- [ ] Run audio DSP on preallocated buffers with no allocation, disk IO, or blocking lock in the
      callback.
- [ ] Add resampling-quality modes and drift correction without changing authoritative timing.
- [ ] Add audio-only scrubbing/blips and J/K/L shuttle behavior with bounded decode requests.
- [ ] Add loudness analysis and normalization as cancellable background jobs.
- [ ] Define plugin-hosting boundaries, but defer third-party plugin loading until the built-in
      graph and crash isolation are proven.

Exit gate:

- [ ] Ten-minute multi-track playback has zero avoidable callback underruns on both reference
      profiles.
- [ ] A/V remains within one project frame after seeks, stalls, rate conversion, and bus changes.
- [ ] Gain, automation, fades, routing, and built-in DSP match exported audio within tolerance.
- [ ] Muting/soloing/routing changes already queued audio on the next safe audio block without
      decoder restart.

### Phase 6 — Sequences, nested timelines, and time remapping

Extend the data model without putting recursive work on the timeline draw path.

- [ ] Add multiple named sequences per project with stable IDs and independent settings.
- [ ] Keep each sequence's timeline/cache/undo ownership isolated.
- [ ] Add sequence tabs/history without loading every sequence’s derived state into the frame loop.
- [ ] Add nested-sequence clips with explicit source sequence and source range.
- [ ] Detect and reject direct or indirect nesting cycles.
- [ ] Flatten only the visible playback contribution on a worker into a bounded preview plan.
- [ ] Add constant speed, reverse, freeze frame, and variable speed ramps.
- [ ] Define deterministic timeline-to-source mapping for ramps and nested sequences.
- [ ] Add optical-flow/frame-blend hooks while retaining nearest-frame fallback.
- [ ] Add pitch policy for audio retiming: varispeed first, stretch modes later.
- [ ] Preserve linked A/V semantics, fades, transitions, markers, and source trims through retiming.

Exit gate:

- [ ] A nested multi-sequence project reopens with exact timing and no cycle ambiguity.
- [ ] Zoom, pan, scrub, move, trim, and razor remain inside foundation budgets regardless of nested
      content complexity.
- [ ] Preview/export use the same time mapping for forward, reverse, freeze, and ramp segments.
- [ ] Undo/redo remains bounded and does not snapshot entire projects.

### Phase 7 — Export/render parity

Export the actual editing engine rather than a simplified timeline interpretation.

- [ ] Feed export from the same sequence, time-mapping, compositor, effect, color, and audio graph
      descriptions used by preview.
- [ ] Preserve immutable snapshot-at-start semantics and display the snapshot revision.
- [ ] Add render range, in/out range, individual clips, and full-sequence modes.
- [ ] Add presets for H.264, H.265 where legally/configurationally available, AV1, image sequence,
      WAV, and high-quality intermediate output supported by the LGPL bundle.
- [ ] Add resolution, frame rate, bitrate/quality, GOP, audio codec/rate/layout, and hardware/software
      encoder controls.
- [ ] Detect encoder capability and retry documented fallbacks without silently changing settings.
- [ ] Add resumable progress reporting by rendered timeline ticks and current stage.
- [ ] Preserve cancel/no-partial-output behavior for every encoder path.
- [ ] Add render queue with bounded concurrency and pause/resume between jobs.
- [ ] Validate output duration, stream metadata, frame count, A/V sync, fades, color, and audio
      loudness against the snapshot.

Exit gate:

- [ ] A multilayer project with transforms, effects, transitions, titles, color, buses, and
      automation exports with preview parity.
- [ ] Editing stays within interaction budgets during background export.
- [ ] Cancellation removes partial outputs and terminates the exact helper/process tree.
- [ ] Hardware encoder failure falls back or reports a precise actionable error.

### Phase 8 — Blick-style overview data and workflow polish

Improve information density without coupling it to live decode.

- [ ] Add a multi-resolution Überblick-style color strip derived after timeline placement.
- [ ] Add MultiWave-style low/mid/high-band overview alongside the existing peak waveform.
- [ ] Store overview pyramids in a versioned disk cache keyed by content fingerprint and analysis
      settings.
- [ ] Invalidate only affected derived ranges after source interpretation changes.
- [ ] Add clip badges for proxy, offline, VFR, hardware/software decode, effects, and cached status.
- [ ] Add relink, replace, reveal-in-folder, consolidate/gather, and unused-media cleanup workflows.
- [ ] Make Project Hub collections persistent and allow projects to be moved/copied between them.
- [ ] Add workspace presets and persist every explicit panel choice without overriding defaults.
- [ ] Complete English/Japanese coverage for all new commands, errors, tooltips, metadata, and
      accessibility labels.
- [ ] Add keyboard remapping and searchable command discovery.

Exit gate:

- [ ] Importing a large folder still queues no heavy analysis until media is used.
- [ ] Overview data appears progressively and never blocks editing or first play.
- [ ] Derived cache deletion causes only regeneration, never project loss or changed output.
- [ ] Every new workflow has keyboard, tooltip, bilingual, persistence, and undo expectations
      documented and tested.

### Phase 9 — Reliability, hardware qualification, and release

- [ ] Run the entire fixture matrix on Intel integrated, NVIDIA discrete, and AMD discrete systems.
- [ ] Test current supported Windows 10 and Windows 11 builds at 100%, 125%, 150%, and 200% DPI.
- [ ] Test single/dual-monitor, monitor disconnect, sleep/resume, device reset, GPU-driver reset,
      audio-device change, and application focus loss.
- [ ] Test 8 GB, 16 GB, and constrained-cache configurations.
- [ ] Test projects with 50,000 bars, thousands of media entries, multiple sequences, offline media,
      and mixed frame rates.
- [ ] Fuzz project JSON, effect graphs, metadata parsing, corrupt media, and drag/drop inputs.
- [ ] Add deterministic crash reports that exclude media content and private paths by default.
- [ ] Add recovery UI for autosave, missing media, unsupported effects, and failed hardware paths.
- [ ] Verify license notices and redistributable codec/driver boundaries for every package.
- [ ] Produce a signed Windows installer only after the portable package passes all gates.
- [ ] Repeat hands-on editing sessions, not only automated smoke tests.

Exit gate:

- [ ] No blocker or high-severity issue remains in the supported Windows matrix.
- [ ] All foundation and roadmap phase gates pass from a clean checkout/package build.
- [ ] No test or failure path leaves Maelstrom, FFmpeg, or export helpers running.
- [ ] The final audit maps every checked feature to source, automated proof, packaged proof, and
      hands-on proof where interaction quality matters.

## Master feature checklist

### Playback and media

- [ ] Concurrent multi-source decode
- [x] Adaptive preview resolution
- [x] Manual preview-quality selection
- [ ] Optional proxies
- [ ] VFR-correct source mapping
- [ ] Broad decode corpus
- [ ] Bounded decoder/session eviction
- [ ] Accurate backend/fallback reporting

### Video compositor

- [x] Ordered multilayer composition
- [x] Position/scale/rotation/anchor
- [x] Crop/flip/opacity
- [x] Fit/fill/stretch/original sizing
- [ ] Correct premultiplied-alpha semantics across every video/image source
- [x] Still-image import, timeline editing, retained preview, and export
- [ ] Sampling-quality choices
- [x] Double-buffered output
- [x] Bounded render-target pool

### Effects, animation, color, and titles

- [ ] Serializable effect graph
- [ ] Worker graph compilation
- [ ] Keyframes and interpolation
- [ ] Core GPU effects
- [ ] Masks and bypass
- [x] Native cross dissolve with preview/export parity
- [x] Native dip to black with preview/export parity
- [ ] Additional video transition types
- [x] Native audio crossfades with preview/export parity
- [ ] English/Japanese titles
- [ ] SDR color pipeline
- [ ] LUTs and scopes

### Audio

- [ ] Track/bus/master mixer
- [ ] Routing and channel layouts
- [ ] Pan, mute, solo, automation
- [ ] Peak/true-peak/RMS/LUFS meters
- [ ] Built-in EQ/dynamics/delay tools
- [x] Equal-power crossfades
- [ ] Shuttle/scrub audio
- [ ] Loudness analysis/normalization
- [ ] Preview/export DSP parity

### Timeline and sequences

- [ ] Multiple sequences
- [ ] Nested sequences and cycle rejection
- [ ] Constant/reverse/freeze speed
- [ ] Variable speed ramps
- [ ] Frame blend/optical-flow hooks
- [ ] Retimed audio policy
- [ ] Bounded inverse undo across new operations

### Export

- [ ] Full compositor/effect/audio parity
- [ ] Range and queue support
- [ ] Additional legal formats/codecs
- [ ] Hardware capability selection and fallback
- [ ] Advanced output controls
- [ ] Output conformance validation
- [ ] Clean cancellation for every path

### Analysis and workflow

- [ ] Überblick-style color overview
- [ ] MultiWave frequency overview
- [ ] Versioned content-keyed disk cache
- [ ] Relink/replace/reveal/consolidate workflows
- [ ] Persistent Project Hub collections
- [ ] Workspace presets
- [ ] Complete bilingual coverage
- [ ] Keyboard remapping and command discovery

### Quality and shipping

- [ ] Integrated Intel qualification
- [ ] NVIDIA qualification
- [ ] AMD qualification
- [ ] Software-only qualification
- [ ] 4K/8K and long-GOP stress
- [ ] Ten-minute playback/export soaks
- [ ] Memory-pressure and cache-bound proof
- [ ] Corrupt/offline media resilience
- [ ] DPI/multi-monitor/device-reset proof
- [ ] Signed Windows package and installer

## Definition of “matched the experience”

Maelstrom may claim a Blick-class engine experience only when all of the following are true:

- [ ] A real multilayer project can play, scrub forward and backward, edit, and export through the
      same compositor/audio/effect semantics.
- [ ] 50,000-bar interaction and full-frame CPU budgets remain green while that project is loaded.
- [ ] Integrated-GPU and discrete-GPU reference machines automatically select usable preview paths.
- [ ] Picture quality can degrade adaptively under load, but the playhead and timeline never wait.
- [ ] Audio stays continuous and synchronized or reports a precise device/media failure.
- [ ] Save/reopen, autosave recovery, undo/redo, and export preserve every supported feature.
- [ ] Long-session, codec-diversity, hardware-fallback, memory-bound, and cancellation tests pass.
- [ ] Hands-on testers describe zoom, pan, trim, move, scrub, and playback as immediate and
      predictable without knowing which backend was active.

Until those checks pass, the honest description is: **Blick-class foundation architecture with a
growing professional engine**, not full Blick-equivalent power.
