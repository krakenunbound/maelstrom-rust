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
- [x] Add generated fixtures for tests that cannot depend on redistributable media, including
      irregular VFR presentation timing and an MPEG-2 transport-stream fixture with observable
      B-frame packet reordering. This corpus gate validates fixture metadata and probe evidence;
      it does not establish preview or export behavior.
- [x] Add an opt-in real-media corpus runner for large local files.
- [x] Record renderer GPU, decoder backend, encoder backend, driver, CPU, RAM, preview scale, cache
      cap, and display refresh in every performance report.
- [ ] Add ten-minute playback soak, repeated reverse scrub, rapid project switching, media-offline,
      memory-pressure, and export-cancellation scenarios.
      A finite prerequisite matrix covers the listed behavioral scenarios and includes a bounded
      350 MiB cumulative /
      280 MiB live runtime strip-cache checkpoint with per-insertion cap and exact oldest-eviction
      assertions, plus a four-source decoded-frame cache-pressure scenario using distinct fixture
      paths that forces real LRU eviction at a three-frame 160x90 RGBA cap and proves release and
      source/session/actor bounds. A separate twelve-source scenario cycles three batches of four
      concurrent decoders through the same three-frame cache, requires at least nine real LRU
      evictions, and proves three eager idle-release cycles with zero final session/source/actor
      ownership; idle/session LRU policy and broader cross-hardware pressure remain open. The exact
      packaged executable passed the 600-second Full/Full real A/V loop soak on 2026-08-30 with 10 loops, 18,023 native
      uploads, zero held/late frames, zero monitor errors/fallback uploads, zero audio
      underruns/lock failures/late discards, and 12.242 ms rolling request-turnaround p95. Its
      schema-5 resource evidence reported a 1,071,555,584-byte peak decoded-frame cache below the
      1 GiB cap, one active foreground session/actor, zero active background sessions, and exact
      session/actor bounds. The schema-2 wrapper recorded 1,038,934,016 bytes of peak GUI
      working-set growth above its warmed baseline, within the deliberately generous 1.5 GiB
      bound. This combined item remains open for broader multi-source memory-pressure coverage,
      including idle/session LRU and cross-hardware proof. See
      `docs/phase0-scenarios.md` and `docs/performance-reports.md`. A separate timed Phase 0
      orchestrator now repeatedly executes the full seven-scenario native matrix and writes one
      versioned report with all available child evidence, aggregate scenario totals, machine/FFmpeg
      identity, source revision, and preserved failure details. Schema 2 requires identical clean
      tracked start/end commits for authoritative status and fails a dirty/unavailable 600-second
      request before the matrix. Runs below 600 seconds are harness checks only. The retained
      authoritative local schema-2 wrapper over the schema-4 matrix on 2026-08-30 embeds clean
      commit `99d43d65d87474f71b83361cec5d5f79a69e4532` and passed for 600.673 seconds,
      excluding 1.133 seconds of setup, with 521 complete seven-scenario runs, 3,647 scenario
      executions, and 19,277 declared work iterations. Every child report had a unique SHA-256 and
      passed; the Software decoder and `h264_mf` encoder were recorded only in their separate
      nullable role fields. The retained report SHA-256 is
      `bef925939b118aaf7d9c1339cbd6e0cfca1c084b0e7b57a46d24971f0ba1e5d6`.
      The current clean-commit requalification at
      `773dd92fa053dcb64f720e34ed7a0e759e38c290` passed 76 complete matrices over
      607.115 measured seconds: 532 scenario executions, 2,812 declared work iterations,
      76 unique child-report hashes, identical clean start/end commits, and no invocation or
      report-read failures. The schema-2 wrapper SHA-256 is
      `E268A39906A88011C79285E8ACD7B26096FB40347D94A6F92D4D99477ED8F3E6`.
      It renewed headless Software decode/`h264_mf` evidence only; adapter inventory is not
      rendering proof, and live-audio, GUI, packaged-playback, scanout, and cross-hardware gates
      remain open.
      Cross-hardware proof remains open.
- [ ] Add per-stage timing for demux, decode, transfer, scale, composite, upload, audio mix, and
      presentation submission without logging per frame in normal builds.
      Decoder-worker aggregates now cover cache lookup, demux, decoder calls, hardware transfer,
      scale, RGBA packing, and the active worker request. Viewer upload, changed-composition encode,
      presentation-call CPU boundaries, and whole audio output-callback plus successful-lock
      mix/render timing are also reported. A bounded, single-in-flight, non-blocking queue callback
      reports submit-to-GPU-completion elapsed time. Optional wgpu pass-boundary timestamps now
      isolate changed viewer-compositor GPU execution with one asynchronous sample in flight;
      unsupported adapters retain the existing path and serialize the stage as unavailable rather
      than zero. Neither GPU metric claims presentation or physical scanout. The schema-7 surface
      report also carries a nested cumulative
      `runtime_diagnostics` snapshot for
      monitor drop/hold/late/error and audio underrun/lock/late-discard counters; these counters
      cover process/session lifetime rather than the fixed 120-frame timing window. Windows package
      validation exercises report structure and required evidence, while numeric thresholds remain
      in the dedicated soak gates. The hybrid Windows host now has both the narrower schema-1
      headless DX12 `ViewerCompositorRenderer` proof and full schema-7 surface qualification for
      Intel UHD 770 `IntegratedGpu` and NVIDIA RTX 3090 `DiscreteGpu`. The full runs exercised the
      ordinary window surface, deterministic A/V import, native viewer uploads, audio callbacks,
      GPU completion/timestamp evidence, and clean cancelled export. The retained schema-1 summary
      SHA-256 is `d1bd17bb3c482de9c7d26c8dc507ff5096961656d0998fa4d9697ccb6541e385`.
      The ordinary bilingual performance-HUD hover now also exposes the existing bounded live
      stages: demux, decode, hardware transfer, scale/RGBA packing, viewer upload, compositor CPU
      encode, optional compositor GPU execution, GPU submit-to-completion, audio mix, and surface
      present-call CPU time. Decoder/audio cumulative aggregates are labeled mean/max; bounded
      viewer/GPU windows are labeled p95/max; every observed row includes its sample count, while
      unsupported or unobserved stages remain explicitly unavailable. It also reports active video
      layers and selected-to-resolved preview quality. The compositor callback snapshot uses a
      non-blocking lock attempt, so HUD collection cannot wait on rendering.
      Physical scanout remains unobserved, so this item stays open.
- [x] Add visible dropped/held/late-frame counters and audio underrun counters to diagnostics.
      A deterministic headless `App::apply_monitor_decode_event` contract now proves the production
      event classifier increments stale/non-converging drops, late/held completions, presentations,
      fallback uploads, and current errors with exact cross-counter invariants. Its test-only
      constructor skips startup-resource loading and native audio initialization; the contract uses
      no window, renderer, decode request, FFmpeg process, or synthetic native-GPU claim.
      Packaged/live soak thresholds remain the authoritative runtime evidence.

Exit gate:

- [x] The full schema-7 surface report can be produced on integrated and discrete Windows hardware.
      The retained 2026-08-30 hybrid-host qualification selected Intel UHD 770 `IntegratedGpu` and
      NVIDIA RTX 3090 `DiscreteGpu` explicitly, rejected adapter-class fallback, and passed all
      schema, CPU, cadence, media, audio, GPU, runtime-counter, and cancelled-export checks. It does
      not prove DWM composition or physical scanout.
- [ ] Existing foundation gates remain green.
      Requalified on 2026-08-30 against the exact Windows package. Workspace tests and strict
      all-target Clippy passed; the release 50,000-bar editor measured 0.472 ms wide-frame p95,
      0.268 ms detail-frame p95, and 0.307 ms playhead p95. The combined real H.264 plus 20,002-bar
      gate measured 0.499 ms input-to-visual p95, and the RTX 3090 timeline shader gate measured
      0.119 ms p95. The package presented its first surface in 698.862 ms, sustained 149.90 surface
      submissions/s with 0.993 ms CPU p95, completed native A/V acceptance and clean export
      cancellation, and passed the full-path launcher check with all 12 adjacent runtime DLLs.
      Packaged `Maelstrom.exe` SHA-256:
      `19859AB6534223B968E236048A7593C9CD4ABFACFD00C3F6CC872A9B842F2348`.
      Reopened by a later 2026-08-30 source-tree check: the existing ignored release
      `fifty_thousand_clip_editor_history_events_stay_under_two_ms` test exceeded its
      unchanged 2 ms edit/release threshold in four runs (3.0755, 2.6410, 2.7171,
      2.6261 ms). Pointer-press checkpoints remained below 2 ms. The ordinary app,
      decoder, and UI-core suites passed; this newly measured history failure is
      separate from the historical package/draw results above. Profile relocation,
      index maintenance, and history capture before changing implementation or budgets.
      See `docs/phase1-generation-stress.md` for retained logs.
      A subsequent relocation fix replaced whole-track sorting/global lookup rebuilds with
      in-place range rotation and localized index updates, and fixed distant-destination
      collision rejection before mutation. Serial release workspace verification passed 707
      tests and strict workspace Clippy passed. The ten-trial history rerun improved move
      p50 to 0.5434 ms and total edit/release p50 to 1.6175 ms, but only six trials passed both
      unchanged 2 ms limits: three press captures and one release exceeded them. Pointer-press
      p95 was 2.2894 ms; edit/release p95 was 2.4516 ms. This gate stays open for checkpoint
      capture/release tail work; see `docs/timeline-relocation-performance.md`. The preceding
      parallel workspace run also exposed a decoder-fixture cleanup sharing violation that
      did not recur in the serial run; deterministic teardown remains to be repaired.
      The next shared-clip checkpoint passes the local release history gate in all ten trials:
      press p95 0.3481 ms and edit/release p95 0.9921 ms, with unchanged 2 ms thresholds and
      undo/redo checks. Snapshots now share immutable clip records and edits copy only touched
      records; flat project JSON is unchanged. Dense wide/detail/playhead CPU p95 is
      0.4656/0.3434/0.3489 ms; cache rebuild plus banding is 0.8655 ms. Eight integration tests
      cover snapshot/effect isolation, normalization, JSON, equality, 50k move/probe detachment,
      and history. Decoder teardown now waits for actor/session retirement before fixture
      deletion; ten focused release trials passed. These source-tree CPU checks do not renew
      packaged/live, cross-hardware, or soak evidence; the broader gate remains open.
      See `docs/timeline-relocation-performance.md` for retained results and ownership tradeoffs.
      Final serial release verification passed 715 tests and strict all-target Clippy passed.
      A parallel rerun nevertheless stalled during a two-second equal-power audio crossfade
      export, emitting non-monotonic AAC timestamps until its exact FFmpeg child was stopped.
      The same test passed before and after that run. Preserve and diagnose this intermittent
      failure using `artifacts/phase1-multisource/shared-clip-export-stall/`; do not treat the
      serial pass as closure of the export reliability gate.
      The clean-HEAD timeline-foundation runner was renewed at `4f4fd5e` on 2026-08-31. Ten
      50,000-clip history trials passed with 0.2565/1.2552 ms press/edit-release p95;
      wide/detail/playhead CPU p95 was 0.4885/0.3386/0.4274 ms; real 1920x1088 H.264 plus 20,002
      bars was 0.5108 ms p95. The schema-1 report hash is
      `36675C22C160CD4A0B90EBAB89DA33F859B50046B0CFCF204BF7FD161DE49399`. This renews the
      source-tree CPU/decode subset only; package, GUI, soak and export reliability keep the broad
      foundation gate open.
      Follow-up reproduced the saved command without Rust: six of twenty runs timed out at
      the unbounded `apad`/timestamp-trim boundary. Final mixed audio and silence now use finite
      48 kHz sample padding/trimming plus a sample-derived clock. Twenty corrected saved-command
      runs and twenty production crossfade tests pass; saved outputs are byte-identical to a
      successful original run. Deterministic missing/negative-clock and empty-input regressions
      fail with the old boundary and pass with the fix. Stderr retention is bounded to 64 KiB,
      and polling errors clean up the exact child/readers. See `docs/audio-export-boundary.md`.
      Three consecutive parallel release workspace runs passed 720 tests each; strict
      all-target workspace Clippy and independent review passed.
      The specific local export failure is repaired; live/cross-hardware/soak gates remain open.
      A versioned clean-commit timeline-foundation runner now repeats the existing 50,000-clip
      history gate ten times, runs wide/detail/playhead CPU evidence, and adds a real H.264 plus
      20,002-bar interaction check without serializing the private media path. Its retained
      schema-1 run at commit `6576d91e25d34f8a3203382d9bd483ffe9e77056` passed with
      0.2672/0.7453 ms press/edit-release p95, 0.4578/0.2900/0.2787 ms
      wide/detail/playhead p95, and 0.4839 ms combined p95. This renews the current headless
      foundation evidence only; GUI-present, packaged, cross-hardware and soak gates remain open.
- [x] A failing codec, driver, or stage is identifiable from one report without guessing.
      The full-surface qualification wrapper now uses schema 2 on both pass and operational
      failure and attempts atomic publication once it owns the report lock. Its failure envelope
      separates stable component/stage, affected codec,
      requested adapter, observed decoder/encoder/renderer backend, renderer driver/driver-info,
      bounded error text, artifact, and exit-code evidence. Evidence that does not exist yet is
      explicit JSON `null`, never inferred. A deterministic incomplete-package fixture proves a
      nonzero run still leaves one `package` / `runtime_closure` report with unknown backend/driver
      fields null; stage-specific validation maps MPEG-4 decode/viewer, AAC audio, and H.264 export
      failures to the codecs actually exercised.

The report's CPU boundaries do not prove GPU completion or scanout. Its optional pass timestamps
measure isolated viewer-compositor execution, and the separate queue callback proves completion of
submitted GPU work as observed by wgpu; neither proves DWM composition or physical scanout.
Physical scanout and broader cross-hardware soak proof remain open and keep the stage-timing item
and remaining Phase 0 exit gates incomplete; soak thresholds remain owned by their dedicated
runners.

### Phase 1 — Multi-source playback and adaptive preview

Turn the single topmost-source monitor into a scheduler capable of feeding a compositor.

- [x] Define immutable `PreviewRequest` data containing sequence generation, playhead tick, output
      size, preview quality, and ordered visible layer/audio-source descriptions. The app now
      captures ordered audible-source metadata in a fixed 64-entry request snapshot, with explicit
      overflow tracking and no cap on actual audio playback.
- [x] Maintain one sticky decoder session per active source/lane within a bounded session pool. A
      shared hard pool now limits all visible monitor decoders to four foreground and four
      speculative-background permits, with exact coherent playback-soak diagnostics and RAII
      release on every exit path. All monitor decoders now also share one app-wide hard-capped
      decoded-frame cache, so identical source/tick/output requests reuse the same pixels and the
      byte budget is measured exactly once. A bounded source coordinator now keys physical actors
      by media/path/acceleration, shares thread-confined foreground/background decoder state across
      logical monitor clients, retains only each client's latest request, defers rather than lies
      at source capacity, and asynchronously retires actors. Real same-source app coverage proves
      independent logical results with one physical session and release after the last consumer.
      The post-refactor authoritative ten-minute four-source gate held four source groups, five
      live actors/sessions under the eight-actor/session cap, zero retiring actors at the final
      sample, and zero sessions after app drop.
- [x] Prioritize visible/top layers and audible lanes; cancel sources no longer contributing. Video
      admission now releases all absent positional slots first, then uses an allocation-free fixed
      array to admit contributing sources by descending priority with the visually topmost layer
      winning ties. A one-source-cap regression proves a top layer wins fresh contention and an
      absent lower layer releases before its upper replacement is admitted. Under active contention,
      the app now discards speculative background lanes first, then yields one complete strictly
      lower-priority physical source group; equal/higher-priority consumers protect a shared source,
      and oldest request ID breaks equal-priority ties. The selected group's logical leases yield
      sequentially without waiting for actor shutdown. Yielding retains each exact latest request
      and last presented frame, and deferred retry uses the same priority/topmost order. Visible
      reverse-scrub lanes are not treated as speculative prewarm. Deterministic coverage proves
      real-media lower-first takeover, permit-safe retry during actor retirement, actor/session
      bounds, shared-source group selection, recency, and an unchanged complete editor audio-target
      snapshot. This is not yet live audio-device continuity proof. Strict priority has no
      age-based fairness bound. Audio remains independently scheduled and is never truncated to the
      diagnostic snapshot cap.
- [x] Add per-source decoded-frame slots so one slow source cannot block other sources.
- [x] Add adaptive full/half/quarter/eighth preview resolution based on measured frame budget.
- [x] Add manual preview-quality override and an honest Auto mode.
- [x] Add background proxy generation as optional derived media, never a prerequisite to edit/play.
      Video media and timeline clips now expose one bilingual nested Proxy Media workflow: generate
      a balanced 720p proxy in the background, cancel it, switch between proxy/original, retry, or
      delete it. `nle-proxy` publishes only complete video-only intra-frame MPEG-4 files through an
      atomic rename, fingerprints the original path/size/mtime plus a profile version, preserves
      VFR timestamp spacing while normalizing the source origin, and kills/waits its FFmpeg child
      on cancellation. A real 1080p irregular-PTS gate preserves source/proxy frame-interval count
      within 1 ms; a separate real gate proves video-only, <=720p, all-intra output. Its disposable
      local-app-data cache is capped at 64 files / 8 GiB, with sparse-file byte-cap proof. Runtime
      selection is applied only where monitor video `DecodeRequest` paths are constructed; audio
      targets, `.nleproj`, source metadata, and export snapshots keep the original. Missing/stale/
      failed/incomplete proxies therefore fall back to the original and never block editing. The
      monitor frame-cache namespace advances whenever routing changes, preventing original/proxy
      pixels from aliasing under one media ID. A proxy decode failure (including concurrent cache
      cleanup by another process) disables the derived route and immediately resubmits the original.
      Regeneration force-replaces the deterministic artifact on its worker; deletion is also an
      owned background job, and a locked-file failure retains the disabled cleanup handle for retry.
      Post-generation reconciliation also retires any older record evicted by the cache cap. The
      default remains original media. Matching local proxies are now rediscovered by the existing
      bounded media-analysis workers after timeline placement/project reopen and offered as ready,
      never automatically enabled. Epoch/path/user-action guards reject stale discoveries; saved
      projects and audio/export keep original paths. Six new regressions include real generated
      proxy save/reopen/explicit-opt-in; 783 release tests, strict Clippy, fixture contracts and
      updated Phase 0 scenarios pass. The rebuilt package contains this behavior (SHA-256 starts
      `F61C4CA8A840478B`, including the lifecycle corrections below); static/runtime checks pass,
      but GUI qualification remains pending.
      Proxy job startup now validates source/tool files and captures the source fingerprint on its
      worker, returning filesystem failures asynchronously. Two regressions fail before/pass after;
      786 release tests and both real-media proxy tests pass. This is not a whole-lifecycle
      nonblocking claim: tool-path resolution and generation/deletion teardown remain open.
      Completion/enable/reconciliation checks now use one owned worker with 64-entry request,
      result and app-ticket bounds. Checking retains original media and both EN/JA menu actions;
      ticket/artifact/path guards reject obsolete replies, and reset invalidates without joining
      the validation worker. Full-cache checks drain across capacity, and repeated cache mutation
      requests another check. Two before-failing regressions pass; 800 release tests, strict Clippy,
      real-media proxy tests, fixture contracts and Phase 0 scenarios pass. No resolution/profile,
      audio/export path, project schema or dependency change. Parent-reviewed; independent agent
      unavailable. Final validation-worker shutdown may still wait on an in-flight OS file call;
      no GUI-latency or whole-lifecycle nonblocking guarantee is claimed.
      Generation/deletion cancellation and project reset no longer take child locks, kill processes,
      or join in-flight proxy workers on the interface. Worker-owned FFprobe/FFmpeg supervision has
      bounded progress/error/event buffers; reset retains bounded cleanup slots, rejects overlapping
      cache mutation, discards obsolete identity and polls idle cleanup at 20 ms. One before-failing
      four-path regression and subprocess flood/silence/full-queue tests now pass. Final evidence is
      804 release tests, both real-media proxy tests, strict Clippy, fixture contracts and Phase 0
      scenarios. Shared tool-path resolution and OS-file-call shutdown bounds remain open.
      Unused Media Pool items retain deferred analysis. This still
      supports one generation job and one 720p profile; portable/external attachment, persistent
      enable choices, generation queues and multiple profiles remain later product work.
      See `docs/proxy-media.md`.
- [ ] Preserve exact source-time mapping for VFR, rational project rates, trims, slips, and reverse
      seeks. Exact reduced FFprobe `avg_frame_rate` ratios now flow through media analysis, playback
      targets, immutable preview requests, request keys, and decoder cache tolerance without
      millihertz rounding. Fractional frame boundaries use the first representable microsecond, and
      unknown timing is neither snapped to an invented 120 fps grid nor allowed to substitute a
      later cached frame. A bounded/cancellable, single-decode-thread, one-million-point decoded
      best-effort timestamp scan now classifies constant, variable, and unknown timing off the UI
      thread. This matches the
      timestamp contract used by the live libav monitor decoder instead of assuming one packet PTS
      always represents one displayed frame. Variable sources retain a
      runtime-only index, suppress the unsafe average-rate seek grid, hold the greatest source PTS
      at or before the logical playhead, and carry each adjacent local span into request/cache
      policy. The deterministic MPEG-4 fixture proves exact `0/40/110/150/240 ms` irregular PTS
      through analysis and preview addressing. A media-free combined regression now trims and slips
      one irregular indexed source, proves exact-boundary and boundary-plus-one mapping through
      forward and decreasing playheads, carries the same PTS/local span into immutable preview
      requests, and holds the final indexed frame from the exclusive source out-point at both 30 and
      30000/1001 project rates. Empty indexes retain CFR fallback. The probe command contract is
      regression-locked to decoded best-effort timestamps, and an opt-in generated reordered B-frame
      VFR source exercises that scan in decoded presentation order, normalizes its nonzero stream
      start to local source time, and gates waveform, software decode, and preview floor/hold
      addressing at every presentation boundary. Export now retains decoder keyframe
      preroll, applies floor sampling before resetting clip-local time, and bounds the graph at the
      planned source range. A real five-color `0/40/110/150/240 ms` source trimmed to `100..240 ms`
      renders source identities `40/110/150/150 ms` at both 30 and 30000/1001 project rates, proving
      preview/export trim-slip identity and the exclusive out-point. Generated ProRes/DNxHR
      MOV files with 7-second origins now pass 20 head/trim/slip/tail/final-frame export
      cases at 30 and 30000/1001 fps: 88 matching source identities/counts and output
      timestamps within one microsecond of decimal rounding. A nearest-frame mutation
      fails both new regressions; the production policy is unchanged. These use the real
      export graph with a test MPEG-4 encoder, not proof of H.264/hardware/color parity.
      775 release tests, Clippy, fixture validation and the updated Phase 0 runner pass.
      See `docs/shifted-vfr-export-parity.md`. Export now also preserves original
      rational rates in background, video, still, title and matte clocks instead
      of rounding to six decimals. Two regressions fail before/pass after; seven
      live FFmpeg clock cases, 777 release tests, strict Clippy, fixture contracts
      and the updated Phase 0 runner pass. A clean-source portable rebuild now
      contains the fix (executable SHA-256 starts `41BD27272A4CFDE7`); previous
      package backed up, runtime hashes/static imports and launcher check-only
      mode pass. GUI/windowed package qualification remains pending.
      See `docs/exact-export-frame-rate.md`. This remains open pending broad real-media/
      cross-backend proof across more codecs, reorder patterns, and containers.
- [x] Correct frame-declared YUV matrix/range conversion in the monitor. A generated DNxHR HQX
      fixture exposed BT.709 frames being interpreted as BT.601 (4,644 differing RGBA bytes in a
      64x48 frame, maximum channel error 41). The corrected software path passes exact independent
      CLI comparisons; transferred hardware frames use the original frame's color metadata, with
      actual hardware parity still unqualified. Retained-scaler matrix/range changes, untagged
      defaults, YUVJ full range, and RGB/alpha preservation are covered. Generated ProRes/DNxHR
      10-bit shifted-VFR MOV plus supplied HEVC Main 10 pass 57 exact frame/seek comparisons.
      Both MOV fixtures also pass app analysis and local preview boundary/hold checks. 743 release
      tests, strict Clippy, seven fixture contracts, and seven Phase 0 scenarios pass locally.
      This does not close broad codec/hardware, HDR, or preview/export color parity gates.
      See `docs/codec-color-qualification.md`.
- [x] Remove local native-resolution planar/NV12 preview color inconsistency.
      Explicit D3D11VA/DXVA2 H.264 and HEVC Main 10 checks exposed 3,350,500
      differing RGBA bytes (maximum channel error 79) at native 1080p despite
      matching small previews. Accurate scaler conversion fixes the GPU-free
      layout regression and passes 152 exact independent software-CLI comparisons
      across native/padded outputs and forward/reverse/final/fresh seeks. Backend,
      fallback, actual hardware transfer, request identity, and timestamp checks
      remain strict. Conversion CPU cost is measured separately; this is local
      default-adapter evidence, not full playback, cross-GPU, HDR, or export parity.
      See `docs/hardware-decode-parity.md`.
- [x] Retain bounded lifetime evidence of successful decode backends independently of latest-wins
      frame events. Cached frames still carry no producer provenance; observations do not relabel
      active-preview or per-media diagnostics. The missed-event regression fails before and passes
      after the app fix. Three paused/prewarmed Full-1080p 40-trial gates pass: four-source scheduler
      p95 189-270 us and coarse matching-frame p95 69-70 ms. These are local Software headless
      measurements, not sustained/windowed or cross-hardware completion. 747 release tests,
      strict Clippy, and independent review pass. See `docs/phase1-latency-comparison.md`.
- [x] Reduce accurate full-resolution conversion cost with bounded slice threading. Large input
      and output frames use two threads on hosts with at least eight available logical CPUs;
      smaller/low-core cases and setup failures retain the existing serial conversion path.
      Filters, accurate rounding, dimensions, and per-frame color metadata remain unchanged.
      400 exact legacy-converter comparisons, 152 Windows hardware timing/pixel comparisons,
      753 release tests, strict Clippy, and independent review pass. Local bicubic full-HD
      scaler p50 falls from 5.13-5.24 ms to 2.75-2.80 ms; three paused Full-1080p latency runs
      record four-source frame-ready p95 63 ms and scheduler p95 153-274 us. This is not a
      global CPU reservation or sustained/windowed/cross-machine qualification.
      See `docs/threaded-monitor-conversion.md`.
- [x] Honor restricted Windows process CPU availability in the scaler's existing guard.
      Rust 1.92 reports 28 CPUs under a four-CPU affinity mask; FFmpeg correctly reports
      four. Bounding the former by the latter restores serial conversion below eight
      available CPUs without changing quality or the two-thread cap. Actual default
      scaler probes pass at 1/4/7/8/28 CPUs; 765 release tests, strict Clippy, 304
      hardware pixel/timestamp cases, and independent review pass. Eighteen Full-1080p
      before/after latency runs pass the existing scheduler gate but do not demonstrate
      faster scrubbing. Restricted-CPU sustained/audio and windowed/cross-machine
      qualification remain open. See `docs/cpu-budget-conversion.md`.
- [x] Suppress presentation events from speculative prewarm commands while preserving
      background decode, cache/session warming, and backend/stage diagnostics. Both
      standalone and coordinated workers retain visible foreground/reverse replies and
      errors. Five regressions, 770 release tests, strict Clippy, repeated focused tests,
      and independent review pass. The preceding four-CPU ten-minute run failed its
      stale/non-converging-event allowance (289 versus 69); its evidence is preserved.
      Subsequent clean-source ten-minute runs pass at four/eight allowed CPUs:
      72,528/86,620 Full-1080p requests with exactly matching presentations, zero
      drops/errors, and submit p95 924/98 us. The separate four-CPU 30-second run
      fails at 1,042 us. Native-audio qualification exposed a five-second video
      clip-coverage defect in its longer-duration harness. A test-only correction
      extends all four clips through measurement plus warmup and asserts exact sources
      and Full resolution before every measured submission. Two new regressions and
      772 release tests/Clippy/formatting pass. The corrected four-CPU 30-second run
      kept all sources active (2,340 requests) but failed at 16,148 us submission
      p95 with one audio callback-lock failure and 480 underrun frames. Archived
      test-only probes localize most submission delay to the actor wake notification,
      not its internal cause. Passing audio and packaged/windowed/cross-machine
      proof remain pending. See `docs/restricted-live-audio-submission.md`.
      See `docs/silent-monitor-prewarm.md`.
- [x] Keep Windows monitor compute workers at their existing base priority rather
      than automatically boosting them on wake. A same-binary four-CPU audio control
      fails at 14,262 us submission p95; disabling boosts passes twice at 89/59 us,
      without changing Full resolution or base/process/UI/audio priorities. Standalone
      and coordinated workers now apply the policy at startup; live-thread regressions
      fail before/pass after. 773 release tests, Clippy and formatting pass. Production
      four/eight-CPU native-audio runs pass at 55/51 us submission p95 with zero audio
      faults. Four-CPU ten-minute resources pass: 56,464 matching requests/presentations,
      74 us submit p95, zero drops/errors, bounded cache/sessions and zero after drop.
      Eight-CPU ten-minute resources also pass on the same production executable:
      72,596 matching requests/presentations, 66 us submit p95, 46 ms frame-ready p95,
      zero drops/errors, bounded resources and clean teardown. Eight temporary
      same-binary 30-second diagnostics (legacy/base/base/legacy at four and eight
      CPUs) repeat submission gains; frame-ready p95 differences reverse direction.
      A 14.6% cycle-rate loss in the first eight-CPU pair is not repeated in the
      reverse-order pair (+3.1%). No consistent readiness penalty, non-inferiority,
      or end-to-end scrub speedup is established. Hooks removed; fresh 773 release
      tests, Clippy and formatting pass. Portable package rebuilt from clean `c603358`;
      executable SHA-256 starts `5DD49EF46A5BEBD6`. All 23 prior package files are
      recoverably archived; only executable/status changed. Pinned runtime hashes,
      15 AMD64 static import inventories, FFmpeg/FFprobe loader checks and the exact
      launcher's check-only mode pass. Smoke remains `not_run`; no editor launch.
      Independent review and windowed/cross-hardware proof remain open.
      See `docs/monitor-worker-scheduling.md`.
- [x] Remove intermediate RGBA packing allocations while preserving exact pixels, alpha,
      transparent-black padding, source identity, and immutable shared-cache ownership. Checked
      single-allocation packing cuts local native-1080p packing p50 from 2.658 to 1.363 ms and
      native-4K from 11.659 to 5.820 ms. 1,296 small-layout legacy comparisons, full-resolution
      and malformed-buffer tests, 152 Windows hardware pixel/timing cases, 764 release tests,
      strict Clippy, and independent review pass. The stage counter now includes final shared
      allocation; historical counters omitted that cost. Three four-source Full-1080p latency
      runs pass at 63-67 ms frame-ready p95, without demonstrated end-to-end scrub improvement.
      Subsequent local resource qualification passes 600.034 seconds / 21,072 four-source
      cycles, with bounded cache/session use, zero errors, and zero post-drop sessions.
      This is not visible real-time playback proof; windowed/lower-core qualification remains
      pending. See `docs/phase1-sustained-soak.md` for the resource evidence and limits.
      See `docs/monitor-rgba-packing.md`.
- [x] Add decode-session eviction that respects the global byte/session cap. The app-wide monitor
      policy reclaims speculative-prewarm actors first and then selects the lowest-priority, oldest
      eligible visual source group, yielding its logical leases sequentially without waiting for
      actor shutdown. The decoder drops actor leases asynchronously through the bounded reaper while
      retaining exact retry work; explicit release still creates no retry. Visible reverse-scrub
      lanes are protected from speculative release. Hard-cap, permit-retirement, and
      post-release-zero tests cover live plus retiring actors. The decoded-frame cache remains
      independently byte-capped by its exact LRU accounting.
- [x] Expose active source backend, preview scale, proxy/original choice, and fallback reason.
      The Inspector now presents a fixed four-layer runtime view of the pixels actually retained
      by the viewer: original source versus the honestly named internal scrub preview, concrete
      decoder backend when observed, selected-to-resolved quality and raster dimensions, and
      structured forced-software, hardware-unavailable, or runtime-hardware-failure reasons in
      English and Japanese. Sticky decoder sessions retain runtime-fallback provenance while
      shared cache hits explicitly report backend and fallback as unobserved rather than borrowing
      a different session's identity. Diagnostics clear with their monitor layer and are excluded
      from `.nleproj`; full decoder, UI-core, and app suites cover fallback retention, cache
      provenance, per-layer lifecycle, localization, and persistence. It now also distinguishes
      the original source, user-selected Proxy Media, and internal scrub preview in English and
      Japanese without conflating the two derived preview paths.

Exit gate:

- [ ] Four independent 1080p sources can be requested concurrently without timeline latency
      regression. A preliminary opt-in local gate now creates four dynamic independent 1080p30
      MPEG-4 sources, submits one explicit Full-output request in under 20 ms, requires all four
      source frames within five seconds, and proves the exact shared 4 foreground + 1 source-owned
      speculative background / 5 peak / 8-cap session state, four source groups, five live lane
      actors, and full post-drop release. It is not a timeline-latency
      regression baseline, p95, sustained, or cross-hardware proof. A second opt-in local gate
      now compares 20 isolated one-source and four-source Full-1080p trials, records nearest-rank
      p50/p95/max scheduler and matching-frame timings, and enforces only a 1 ms headless
      input-to-submit scheduler p95. It remains local Software-backend evidence rather than sustained, UI-present,
      or cross-hardware completion. See `docs/phase1-multisource.md` and
      `docs/phase1-latency-comparison.md`. An opt-in windowed ruler-input harness now correlates
      exact four-source uploads through native compositor blits, records forty measured inputs
      after eight warmups, and prepares fresh one/four-source cases for both adapter classes.
      Authorized local windowed runs now pass both CPU limits on Intel UHD 770 and RTX 3090:
      input p95 0.3178–0.3596 ms and frame CPU p95 0.8291–1.3925 ms. App completion now shares
      the decoder's one-microsecond timestamp-rounding rule. Fresh four-source image p95 remains
      431–474 ms with Software decoding; reference-machine, broader backend/media, and sustained
      playback qualification remain open. See `docs/phase1-ui-qualification.md`; the dedicated
      workload does not replace the package's general smoke suite.
      A follow-up nearest-keyframe seek with bounded preroll fallback reduces local four-source
      matching-frame p95 to 74–104 ms in two clean final-package repetitions, with CPU limits
      unchanged. Pixel/reference and packet-work regressions also cover reordered media and
      final delayed B-frames; 735 release tests pass. Two additional workload-integrity
      invalidations remain disclosed and unproven in origin. See `docs/scrub-seek-performance.md`.
      A deterministic real-egui test now reproduces incoming pointer motion changing the held
      benchmark drag. Failed samples retain their original target/identity evidence, plus a
      bounded input summary; native input is not suppressed and no acceptance rule is relaxed.
      One further four-case windowed run and 737 release tests pass. Historical input origin
      remains unproven; see `docs/windowed-input-integrity.md` for evidence and limits.
      A third opt-in headless local gate now repeats the
      same four-source Full-1080p five-second-fixture forward/back scrub workload for a bounded
      duration (600 seconds by default), with raw scheduler/frame-ready samples, exact runtime
      counter deltas including a documented bounded stale-event allowance, bounded cache/session evidence, post-drop release, and tracked-process
      working-set samples. It is not realtime playback, audio, visible UI, GPU compositor, or
      cross-hardware proof. The committed gate passed its authoritative local Software run on
      2026-08-29 after the app-wide cache consolidation for 600.032 seconds: 14,899 cycles / 59,596
      requests, 38 us scheduler p95 (2,675 us max), 49 ms coarse frame-ready p95 (76 ms max), 6
      rejected stale events within a 60-event bound, zero errors, 207,360,000 current / 215,654,400
      exact peak bytes under the 1 GiB cache cap, 7 peak sessions under the 8-session cap, zero
      post-drop sessions, and 85,987,328 bytes of working-set growth under the 1.5 GiB diagnostic
      bound. That historical run predates source-owned actor/session deduplication. The isolated
      shared-cache comparison held four-source scheduler p95 to 71 us; the post-source-actor rerun
      passed at 144 us with a 78 ms coarse frame-ready p95. The post-source-actor
      authoritative run then passed for 600.020 seconds with 16,494 cycles / 65,976 requests,
      44 us scheduler p95 (416 us max), 45 ms frame-ready p95 (64 ms max), seven bounded stale
      events, zero errors, five peak sessions/actors under the cap of eight, four live source
      groups, zero final retiring actors, zero post-drop sessions, and 40,943,616 bytes of
      working-set growth. After exact rational source-rate propagation, the bounded four-source
      gate passed again on 2026-08-30 at 125 us submission / 75 ms all-frames-ready, and the
      interleaved 20-trial comparison passed at 140 us scheduler p95 / 82 ms frame-ready p95.
      A fourth opt-in headless gate now opens the real default audio output, proves consumed
      nonzero PCM only after transport warmup, and keeps that native device clock continuous while
      four independent Full-1080p requests remain active. Its 2026-08-30 local Software-backend run
      passed for 5.002 seconds with 500 callback/mix samples, 5,000,000 us of device-clock advance,
      1,813 us wall/device drift, a 22 ms maximum progress interval, 98 submissions / 388 accepted
      and presented layer frames (97 per source), 153 us scheduler p95, nonzero meter evidence in
      all 1,632 measured observations including the final sample, zero post-warmup underruns,
      callback lock failures, late audio discards, or monitor errors, and zero post-drop sessions.
      The schema-2 gate now also stalls the exact next topmost real-media decoder request for 750 ms
      while its worker remains live. Its 2026-08-30 rerun recorded 45 ready-source presentations and
      750,000 us of native audio-clock advance during the hold, then 82 delayed-source presentations
      after release; it completed 373 monitor presentations with 1,688 us clock drift, a 22 ms maximum
      progress interval, zero monitor/audio faults, and zero post-drop sessions. The deterministic
      barrier is compiled only by the test-only `nle-decode/test-hooks` feature.
      Coverage caveat discovered 2026-08-31: historical audio fixtures had only five
      seconds of video timeline coverage, including warmup. Their audio counters and
      early barrier observations remain evidence, but they do not prove sustained
      four-video load after that boundary. The test now extends video coverage and
      asserts all four sources per submission. Corrected four-CPU qualification
      exposes submission and audio failures; see `docs/restricted-live-audio-submission.md`.
      The maximum-duration corrected gate was requalified on 2026-08-31 at tracked-clean
      commit `f78c816`: 30.007 seconds with all four Full-1080p sources, a real default
      audio device, 46 us input-submission p95, 7,138 us device-clock drift, 45 ready-source
      presentations and 750,000 us of audio-clock advance during the deliberate 750 ms hold,
      572 delayed-source presentations after release, and zero monitor errors, audio
      underruns, callback lock failures, late audio discards, or post-drop sessions. The
      ignored schema-2 report SHA-256 is
      `AA378E44407BEA9C39989B8024561492880D8DB30C9A11EC42DE637EBCD52F07`.
      The exit remains open for realtime UI-present and cross-hardware proof; see
      `docs/phase1-sustained-soak.md` and `docs/phase1-live-audio.md`.
- [x] A deliberately slow source cannot delay a ready source or the playback clock. The bounded
      schema-2 live-audio gate now combines four independent Full-1080p real sources, the native
      default audio device, and a requested 750 ms topmost-source worker hold. Other real sources keep
      presenting, the device clock keeps advancing with nonzero consumed PCM and no underruns, and
      the delayed source recovers after release. This is local Software-backend evidence; the
      separate four-source UI-present/cross-hardware exit gate remains open.
- [x] Rapid layer enable/disable and backward scrubbing publish only the latest generation.
      A deterministic app sequence covers forward/backward scrub, disable, re-enable, and newest
      re-enabled generation/request presentation while an unaffected layer remains retained;
      the opt-in real-media gate now adds 32 rotating-layer cycles across four independent
      dynamic 1080p30 MPEG-4 inputs with explicit 640x360 output. It forces one real decoder
      supersession, rejects a captured real frame after same-media re-enable, and accepts a
      control changing only its generation. All 33 stale replays are rejected; each cycle's
      actually accepted final event must match the latest generation/request, and transient
      obsolete acceptance during recovery fails immediately. The final 2026-08-30 local
      Software run passed in 1.73 seconds with zero current errors, 96 resource checkpoints,
      a 246,988,800-byte peak cache below 1 GiB, five peak sessions below eight, and zero
      post-drop session/source/actor ownership. This is headless correctness evidence, not
      Full-1080p output, UI latency, audio continuity, ten-minute soak, or cross-hardware proof.
      See `docs/phase1-generation-stress.md`.
- [x] Cache/session memory remains inside its configured hard limit during a ten-minute stress run.
      The current-source 2026-08-30 local Software-backend gate passed for 600.042 seconds:
      15,933 four-source Full-1080p cycles, 63,732 requests, 54 us scheduler p95, zero current
      decode errors, and twelve stale rejections within a 64-event bound. Exact cache peak
      was 215,654,400 bytes under 1 GiB; exact session peak was five under eight, with zero
      active sessions after app drop. Raw distributions, counts, limits, working-set samples,
      and fixture/binary/report hashes were independently checked. Sampled working-set growth
      was 43,008,000 bytes under a diagnostic 1.5 GiB allowance, not a whole-app RAM hard cap.
      Requalified after accurate threaded conversion on 2026-08-31 at source `b3e9228`:
      600.047 seconds, 20,184 four-source Full-1080p cycles, 80,736 requests, 77 us scheduler
      p95, 42 ms coarse frame-ready p95, zero monitor errors, and zero post-drop sessions.
      Cache peak remains 215,654,400 bytes under 1 GiB and session peak five under eight;
      sampled working-set growth is 19,812,352 bytes under the diagnostic allowance.
      Raw samples, counts, and input/binary/report hashes were checked independently.
      This remains local headless Software evidence; windowed/audio/cross-machine gates
      and lower-core contention qualification remain open. See `docs/phase1-sustained-soak.md`.
      This closes the configured cache/session limits for the local headless workload, not
      UI-present/cross-hardware/audio qualification or continuous/post-drop actor accounting.
      See `docs/phase1-sustained-soak.md` for exact evidence and limitations.

### Phase 2 — Real-time GPU compositor

Replace “topmost clip wins” with a generation-aware retained compositor.

- [x] Add bounded compositor-owned render-target/texture reuse and retained scratch storage. Exact
      source/canvas sizes reuse complete layer bundles and double-buffered output pairs across
      resize, source visibility, and temporary no-frame churn. The free pool is capped at four
      layers, one output pair, and 32 MiB of logical payload with oldest eviction; oversize/4K
      bundles are released, and full clear purges the pool. Fixed CPU vertex/matte/count scratch and
      existing GPU buffers are retained. The compositor creates no command buffers because
      `egui_wgpu` owns the callback encoder, so no competing command-buffer pool is introduced.
      Queued real-GPU reuse/readback and accounting regressions pass. This is not physical-VRAM,
      memory-pressure, practical-4K, or cross-adapter evidence. See
      `docs/compositor-resource-pool.md`.
- [x] Upload or import one latest-ready texture per contributing source without per-clip texture
      creation.
- [x] Composite ordered video layers with transparent empty regions over project background black.
- [x] Implement position, scale, rotation, anchor point, crop, opacity, and horizontal/vertical flip.
- [x] Implement project-size fitting modes: fit, fill, stretch, and original pixels.
- [x] Implement premultiplied-alpha handling and correct image/video alpha semantics. Straight
      decoded/generated RGBA is retained at CPU, upload and FFmpeg-overlay boundaries. The viewer
      runs a retained encoded-sRGB premultiply pass only after changed uploads, uses explicit
      premultiplied source-over blending, and selects transfer-correct sRGB/non-sRGB presentation.
      Export premultiplies only around filtered scale/rotation and restores straight RGBA before
      explicit `overlay=alpha=straight` boundaries. Real GPU edge/presentation and generated FFmpeg
      edge, transform, cross-dissolve, title and matte regressions pass. This does not complete the
      Phase 4 linear working space, texture pooling, memory-pressure or cross-adapter gates. See
      `docs/premultiplied-alpha.md`.
- [x] Add still-image layers with bounded texture downscaling.
- [x] Add nearest/bilinear/bicubic preview sampling options where supported. The Bicubic default is
      independent from moving/paused/Auto resolution and export; legacy projects migrate the old
      high-quality boolean. Monitor requests, decoded-frame caches and sticky/hardware-transfer
      scalers carry explicit FFmpeg point/bilinear/bicubic identity. The retained viewer uses
      nearest/linear samplers plus a manual alpha-safe Catmull-Rom shader, with setting-only
      recomposition and no upload or pipeline rebuild. EN/JA state/migration, cache/scaler, WGSL,
      full release workspace, strict Clippy and real-GPU edge/readback regressions pass. Practical
      four-layer 4K, physical scanout and broad cross-adapter performance remain open. See
      `docs/preview-sampling.md`.
- [x] Double-buffer viewer outputs so graph execution never blocks timeline drawing.
- [x] Reuse compiled pipelines and bind groups; no shader/pipeline compilation during playback.
- [x] Report composite time, active layer count, and selected preview scale. The bilingual live
      HUD reports separately named compositor CPU-encode and optional isolated GPU-pass p95/max
      windows, exact sample counts, active contributing video-layer count, and selected-to-resolved
      preview quality. Missing GPU timestamp support is shown as unavailable rather than zero, and
      the render-callback timing snapshot never blocks the UI thread.

Exit gate:

- [x] At least four transformed 1080p layers composite correctly on the discrete reference profile.
      The 2026-08-31 schema-3 headless DX12 qualification pre-uploaded four 1920x1080 sources,
      used Bicubic sampling, excluded five warmups, measured 30 changed generations, and matched
      deterministic RGBA readback on RTX 3090. The final run measured 0.0904 ms CPU encode p95 and
      0.2765 ms GPU pass p95 against the 33.333 ms frame budget. This is offscreen compositor evidence, not app
      presentation, DWM or physical scanout. See `docs/performance-reports.md`.
- [ ] At least two layers operate on the integrated reference profile using Auto preview quality.
      The same qualification passed the two-layer compositor prerequisite on Intel UHD 770 at
      0.1997 ms CPU / 6.9053 ms GPU p95, but explicitly did not exercise app Auto selection or
      presentation; this gate remains open.
- [x] Disabling a layer or changing a transform appears on the next available preview generation.
      The schema-3 cross-adapter gate runs four post-measurement state transitions on the same
      retained production renderer. Changing only the top transform recomposes the next generation
      off-center with no upload while retaining that layer's composition serial; disabling it in
      the following generation removes it with unchanged upload serials. Intel and NVIDIA exact
      center readbacks prove the moved layer left the center, while a second probe inside its new
      quad proves it rendered at the transformed location. The renewed 32-cycle real-media
      generation stress also proves
      disable/re-enable compaction, newest-generation acceptance, and stale replay rejection. This
      is headless compositor/scheduler evidence, not window presentation or scanout. See
      `docs/performance-reports.md` and `docs/phase1-generation-stress.md`.
- [x] Missing/late layers hold or become transparent without stalling ready layers or input.
      The schema-3 GPU sequence clears the declared top source texture, composes the ready lower
      layers with matching readback and current serials, then re-uploads the late source and
      restores the full composition on the next generation. Separately, the current real-media
      missing-upper regression presents its lower layer while the upper source stays absent; the
      renewed four-source stress supersedes a deliberately blocked request and preserves unaffected
      layers with zero current monitor errors. This does not claim physical presentation.
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
- [x] Bounded decoder/session eviction
- [ ] Accurate backend/fallback reporting

### Video compositor

- [x] Ordered multilayer composition
- [x] Position/scale/rotation/anchor
- [x] Crop/flip/opacity
- [x] Fit/fill/stretch/original sizing
- [ ] Correct premultiplied-alpha semantics across every video/image source
- [x] Still-image import, timeline editing, retained preview, and export
- [x] Sampling-quality choices
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
