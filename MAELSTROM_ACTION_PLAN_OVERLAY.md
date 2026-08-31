# Maelstrom action plan (overlay)

**Repo:** https://github.com/krakenunbound/maelstrom-rust
**Commit reviewed:** `80a78327` (main, Aug 2026)
**This document:** 28 August 2026 (includes crate-supersession + improve/redo list)
**Audience:** you + local agents

This is **not** a greenfield crate-shopping list. Maelstrom already has a Blick-class Windows foundation. Do not replace `nle-decode` / `nle-timeline` / `nle-render` with `avio` or `ff-*`. That would be a regression.

Canonical in-repo docs stay authoritative:

- `ARCHITECTURE.md` — hot path, workers, caches, packaging
- `ENGINE_ROADMAP.md` — phases 0–9 and stop gates
- `RUST_NLE_FOUNDATION_PLAN.md` — budgets and v1.1 model
- `FOUNDATION_AUDIT.md` — what is proven

If this overlay and `ENGINE_ROADMAP.md` disagree, `ENGINE_ROADMAP.md` wins.

---

## 1. What Maelstrom already is

Native Rust NLE. `winit` + `wgpu` + egui chrome. Timeline is **one custom canvas**, not a widget tree. UI thread never talks to FFmpeg. Decode, waveforms, audio, save, export live on workers. Generation-tagged results; stale work is dropped.

Honest label: **Blick-class foundation with a growing professional engine.** Not full Blick / Premiere / Resolve. Do not inflate it.

### Workspace crates (do not flatten these)

| Crate | Job |
|---|---|
| `nle-app` | Window, events, orchestration, GPU upload |
| `nle-ui-core` | Hub/editor state, GPU-neutral timeline primitives |
| `nle-timeline` | Edit kernel, inverse undo, visibility/banding |
| `nle-compositor` | Immutable composition requests + geometry |
| `nle-render` | Retained wgpu pipelines, instance buffers |
| `nle-decode` | Sticky FFmpeg monitor sessions, HW + software |
| `nle-waveform` | Probe, peaks, thumb strips, duration |
| `nle-audio` | CPAL, mix, fades, meters |
| `nle-project-io` | Versioned `.nleproj`, atomic writes |
| `nle-title` | CPU titles against bundled Noto Sans JP |
| `nle-export` | Background H.264/AAC, cancel, fallback |
| `nle-cache` | Bounded caches |
| `nle-upscale` | Optional RTX VSR hook (not in public git) |

### Locked stack (Cargo.toml)

- `egui` / `egui-wgpu` / `egui-winit` **0.35.0**
- `wgpu` **29.0.4**
- `winit` **0.30.13**
- `ffmpeg-next` **8.1.0**
- `cpal` **0.15.3**
- `rfd` **0.17.2**
- `image` **0.25.10**
- `glam` **0.33.5**
- edition **2024**, resolver **3**
- workspace license **MIT OR Apache-2.0**
- packaged FFmpeg **8.1 LGPL shared**

Do not bump egui to 0.36 or wgpu to 30 inside a feature PR.

### Already shipped

Splash + Project Hub (EN/JA); media pool; offline magenta bars; 3V/3A tracks; live preview cap of 4 visible unmuted video layers; linked A/V; razor / slip / range / pointer; hold-C razor; gain, mute, fades, equal-power audio crossfades; cross dissolve + dip to black; adaptive preview; transforms (pos/scale/rot/anchor/crop/flip/opacity, fit/fill/stretch); brightness/contrast keyframes; native titles (EN+JA, same raster for preview and export); autosave; relative+absolute media; inverse undo; Quick Export H.264/AAC with HW fallback; 50k-clip CPU layout gate.

---

## 2. Generic crates vs Maelstrom — what is superseded

A lot of this is already **better than the generic crates** for an NLE. Those crates do not know about the playhead, generations, the 50k-clip draw path, or preview/export parity. Do not “upgrade” by swapping them in.

### Keep yours (generic crate is the wrong layer)

| Named earlier | Maelstrom owns | Verdict |
|---|---|---|
| `avio` timeline + undo | `nle-timeline` | **Superseded.** avio is a generic clip list. Yours is integer µs ticks, inverse ops, visibility/banding, 50k budget. |
| `ff-preview` | `nle-decode` + app clock | **Superseded.** Sticky sessions, coalesced seeks, HW fallback, generation discard. ff-preview is a player, not a multilayer monitor. |
| `ff-render` | `nle-compositor` + `nle-render` | **Superseded for current compositing.** Layers + transforms on retained wgpu. Do not import someone else’s graph. |
| `ff-decode` / `ff-encode` / `ff-pipeline` | `ffmpeg-next` 8.1 + `nle-decode` / `nle-export` | **Do not switch FFmpeg families.** A second wrapper is two ABIs and two build stories. |
| `undo` / `undoredo` | inverse history in `nle-timeline` | **Superseded.** A second stack will desync from clip IDs. |
| `glyphon` + `cosmic-text` for titles | `nle-title` + Noto Sans JP | **Superseded for current titles.** One raster so preview and export match. glyphon is GPU UI text; export would drift. |
| `egui_dock` as the app shell | hub + editor chrome + painter timeline | **Not needed to ship.** Optional later for inspector panes only. |

`rfd`, `cpal`, `egui` 0.35, `wgpu` 29, `serde` are the floor. Keep them. They are not superseded.

Agents: if a prompt says “add avio,” reject it.

### Still useful later — only for jobs no crate owns yet

| Need | Take | Do not take if |
|---|---|---|
| Sample-rate convert on audio worker | `rubato` 5 | FFmpeg / `nle-audio` already resamples well enough — measure first |
| Color management (Phase 4) | `ocio-rs` 0.2 bundled/real, not stub | Rec.709 matrices would suffice first |
| Pitch-preserve stretch (Phase 6) | Rubber Band or `timestretch` | varispeed is not done — do that first. Rubber Band is GPL. |
| CLAP host (end of Phase 5) | `clack-host` / study `maolan-engine` | built-in EQ/comp/limiter is not done |
| Utility docking | `egui_dock` 0.20.x **for egui 0.35** | timeline starts living in dock tabs |
| Overview disk cache (Phase 8) | `moka` or a tiny owned file cache | a third cache crate per feature |

**Never take:** avio, ff-preview, ff-render, ff-pipeline, reelforge, gstreamer-rs, Tauri, iced, a second FFmpeg wrapper.

---

## 3. Constraints

1. Native Rust. No Python, no web UI, no Tauri.
2. **Windows + Linux only.** macOS out. Packaged/audited product today is Windows. Linux is a later first-class port after Phase 0–2 stay green — not a second editor this month.
3. **English + Japanese only.** Existing string table + bundled Noto Sans JP. No extra locales.
4. **Not a store product**, but the tree is still **MIT/Apache app + LGPL FFmpeg 8.1**. Hobby does not silently mean x264 GPL.
5. UI thread: no FFmpeg, no disk, no graph compile.
6. Picture quality may drop. Timeline must not wait.
7. Live preview resolves at most **four** visible unmuted video tracks. The monitor-cache byte cap is split across those slots, not quadrupled. That cap is a budget, not a bug to “fix” by decoding twelve streams on the UI thread.

---

## 4. Areas to improve or re-do

Finish holes. Do not replace working crates.

### Must not regress

- UI thread: no FFmpeg, no disk, no per-clip widgets, no walking every project clip to draw a short interval
- Generation-tagged workers; drop stale results
- Timeline O(visible); ≤2 ms / 50k interaction; ≤8 ms p95 CPU frame at 1920×1080
- `.nleproj` is source of truth; frames, waveforms, decoder IDs, GPU handles are not project state
- Preview and export share the same plan for transforms, fades, titles, dissolves
- 4-layer live preview / Quick Export bound stays until the scheduler and cache prove they can do more
- `nle-timeline` stays independent of FFmpeg, wgpu, and egui

### Incomplete — finish, don’t replace

1. **Multi-source scheduler.** Immutable multi-source requests, shared source actors, hard caps, and speculative-prewarm-first priority/recency eviction now exist. Visible reverse-scrub work is protected from speculative release. A bounded real-media/native-audio gate now proves a 750 ms delayed source does not stall ready sources or the playback clock. The remaining gate is UI-present/cross-hardware proof that four 1080p sources do not hitch the timeline.
2. **VFR / rational rates.** Runtime mapping now uses a bounded decoded best-effort timestamp index
   across trim, slip, reverse, and 30000/1001 project rates. Export now preserves keyframe preroll
   and has a real irregular-frame trim/slip identity gate at 30 and 30000/1001. Broad
   real-media/cross-backend qualification remains open.
3. **Proxies.** The first optional derived-media slice is complete: cancellable background 720p
   generation, bounded disposable cache, nested EN/JA media/clip menus, monitor-only selection, and
   original fallback. Proxies never replace project, audio, or export source truth. Queues,
   persistent attachment, and multiple profiles are later extensions, not prerequisites to edit.
4. **Premultiplied alpha.** Layers composite; image/video alpha semantics are unfinished. Redo the blend path, not the compositor crate.
5. **Effect graph.** Brightness/contrast keys on a clip are not a node graph. Design stable IDs, ports, and schema-versioned serialization before a catalog of sliders.
6. **Color.** No working-space pipeline, no LUTs, no scopes. Rec.709 SDR first. Do not drop OCIO onto a display-referred preview.
7. **Audio engine.** Lane mixer + equal-power fades is a start. Missing: buses, solo, pan, channel layouts, LUFS / true-peak, callback-safe DSP, shuttle audio. No alloc / disk / blocking lock in the device callback.
8. **Speed / remap.** No reverse / freeze / ramp as a first-class mapping. Varispeed before any stretch library.
9. **Sequences / nesting.** One timeline today. Isolate sequence ownership before nested clips or the draw path will clone the world.
10. **Export parity.** Quick Export lowers a *bounded* plan (≤4 video tracks; some stills/effects rejected). The redo is “export the same graph as preview,” not a new encoder crate. H.264 prefers Windows HW, then Media Foundation / OpenH264.
11. **Measurement (Phase 0).** HUD has CPU submit stats. Still need a versioned fixture manifest, 10-minute soaks, stage timers (demux / decode / transfer / scale / composite / upload / mix / submit), and drop / hold / late / underrun counters. Without that, every new effect is folklore.

### Soft / later polish (not a rewrite)

- Blick-style color strip + MultiWave (derived cache, never live decode)
- Relink / consolidate / persistent hub collections
- Keyboard remapping
- Linux package script (same crates, LGPL `.so`)
- Sampling-quality choices (nearest / bilinear / bicubic)
- More transitions (wipe, push, dip to white)

### Do not redo

- Timeline widget toolkit
- FFmpeg wrapper family
- Title shaper (unless live GPU-animated text is required; then any GPU path is viewer-only and export still goes through `nle-title`)
- Undo model
- Project JSON as a database

---

## 5. What to do next

Follow `ENGINE_ROADMAP.md`.

### Now — Phase 0 (measurement)

- Versioned fixture manifest (codec, rate, GOP, expected failure)
- Soak: 10 min play plus a separate seven-scenario native matrix for reverse scrub, project switch,
  offline media, cache pressure, twelve-source pressure/eager idle release, and export cancel. The
  provenance-qualified schema-2 wrapper over the schema-4 matrix passed 521 complete runs over
  600.673 seconds; broader cross-hardware soak evidence remains open.
- Per-stage timers and drop/hold/late/underrun counters are now available from the compact HUD
  hover in English/Japanese, with truthful mean-versus-p95 labels, sample counts, active layers,
  preview scale, and explicit unavailable GPU rows. Physical scanout is still not measured.

Gate update: full schema-7 surface reports now pass on integrated Intel and discrete NVIDIA GPUs on
the hybrid Windows host. Physical scanout and broader cross-hardware soak proof remain open.
Foundation 50k + package gates must stay green.

### Then — Phase 1 holes

Done-ish: adaptive preview, manual quality, per-source slots. The Inspector now reports factual
per-layer source kind, decoder backend, selected/resolved scale and dimensions, and structured
fallback state in EN/JA; cache hits remain explicitly unobserved rather than inheriting unrelated
session provenance.
Open: UI-present/cross-hardware multi-source proof, later proxy queues/persistent attachment/multiple
profiles, and broad VFR/cross-backend proof. A bounded local headless gate now proves real-default-device
audio clock and consumed-PCM continuity under four concurrent Full-1080p Software decode sources,
including a deterministic 750 ms topmost-source hold with ready-source progress and delayed-source
recovery; cross-hardware and UI-present audio proof remain open. Bounded speculative-prewarm-first priority/recency session
eviction is implemented; strict priority has no age-based fairness guarantee.

Additional local Software coverage now includes shifted-VFR ProRes/DNxHR 10-bit MOV and supplied
HEVC Main 10. Independent CLI pixels exposed and verified a fix for DNxHR BT.709 preview being
converted with BT.601. These are exact small-frame timing/pixel checks, not hardware/HDR/playback
qualification; see `docs/codec-color-qualification.md`.

Gate: four 1080p sources requested at once; timeline latency unchanged; slow source cannot stall the clock.

### Then — Phase 2 holes

Open: integrated-GPU two-layer Auto proof plus broader compositor memory/performance qualification.
Premultiplied alpha, independent preview sampling, and the composite-time HUD are implemented; see
`docs/premultiplied-alpha.md`, `docs/preview-sampling.md`, and the authoritative roadmap evidence.

### Parallel-safe only if schema/playback ownership is untouched

- Linux as `scripts/package-linux.sh`, not a rewrite of `nle-decode`
- EN/JA strings for new commands only

### Do not start yet

Effect-graph catalog, OCIO, Fairlight buses, nested sequences, optical flow, CLAP, MultiWave.

---

## 6. Linux port (when Windows Phase 0–2 are green)

Same crates. Bundle LGPL FFmpeg 8.1 shared libs. Wayland + X11. cpal → Pulse/ALSA/PipeWire. Same `.nleproj`. Windows stays the blocking CI gate. Linux CI is compile + probe a fixture until Windows soaks exist.

---

## 7. License

- App: MIT OR Apache-2.0
- FFmpeg: pinned 8.1 **LGPL shared**; package scripts reject GPL/nonfree
- Export: OpenH264 / Quick Sync / NVENC / AMF — not libx264 GPL

Hobby status does **not** auto-enable GPL. Rubber Band is GPL; varispeed first. If you ever want GPL encoders, change notices + package gate + the roadmap LGPL invariant in **one** commit.

---

## 8. Agent rules

1. Do not add Python, Node, WebView, Tauri, iced.
2. Do not add `avio`, `ff-preview`, `ff-render`, `ff-pipeline`, or a second FFmpeg wrapper.
3. Do not call FFmpeg, disk, or graph compile on the UI thread.
4. Do not allocate per visible clip on the timeline draw path.
5. Do not bump `egui` / `wgpu` / `ffmpeg-next` in a feature PR.
6. Do not add UI languages other than `en` and `ja`.
7. Do not add macOS code or CI.
8. Do not enable GPL FFmpeg or Rubber Band unless license docs and package gate change in the same PR.
9. Every async result needs project/sequence/source/generation IDs. Drop stale generations.
10. New work needs a stop gate from `ENGINE_ROADMAP.md`.
11. Persistence is `.nleproj` via `nle-project-io`. Derived media is not project state.
12. Preview and export share the same description of transforms, fades, titles, transitions.
13. Do not replace a working `nle-*` crate with a generic crates.io lookalike.

---

## 9. Next 14 days

1. Read `FOUNDATION_AUDIT.md` so agents stop re-proving finished work.
2. Phase 0 fixture manifest + soak + stage timers.
3. Phase 1 session pool / eviction / four-source request.
4. Premultiplied alpha + inspector “why this scale / why this decoder.”
5. Then either a Linux compile job or Phase 3 graph schema — not both.

---

## 10. Sources

- https://github.com/krakenunbound/maelstrom-rust
- https://github.com/krakenunbound/maelstrom-rust/blob/main/ARCHITECTURE.md
- https://github.com/krakenunbound/maelstrom-rust/blob/main/ENGINE_ROADMAP.md
- https://github.com/krakenunbound/maelstrom-rust/blob/main/RUST_NLE_FOUNDATION_PLAN.md
- https://crates.io/crates/ffmpeg-next (pinned 8.1.0)
- https://crates.io/crates/egui (pinned 0.35, not 0.36)
- Blick UX reference only: https://blickeditor.com
