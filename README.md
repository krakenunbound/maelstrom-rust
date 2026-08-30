# Maelstrom — Rust NLE foundation

Implementation status against the foundation plan, now updated for **v1.1**.

The completed foundation evidence is in `FOUNDATION_AUDIT.md`. The dependency-ordered plan for
building the professional multilayer engine is in `ENGINE_ROADMAP.md`.

Current product scope is Windows. Linux is an optional later milestone after the Windows editor
is polished. macOS is out of scope and is not a supported, tested, or completion target.

## Canonical workspace

`H:\Maelstrom Rust` is the single Git and Cargo workspace root. Product source lives in
`crates`, shared assets in `assets`, reproducible tooling in `scripts`, and the one user-facing
Windows package in `dist\Maelstrom-Windows-x64`. `Launch-Maelstrom-Editor.bat` targets that
package by its full absolute path. Cargo's `target` tree and `test-build` packages are disposable
generated output and are intentionally excluded from source control. In particular, executables
under `target\**` (including Cargo's hash-named dependency executables) are unsupported generated
artifacts, not editor launch targets.

The current runnable slice includes the native splash, local Project Hub, and
the first functional editor foundation. Local project documents now persist
media references, timeline edits, and workspace layout between sessions. A versioned model
manifest is loaded on the startup-resource worker after the first splash frame; listed model bytes
remain resident for the future inference engines.

## Run

```powershell
& 'H:\Maelstrom Rust\Launch-Maelstrom-Editor.bat'
```

This is the supported editor entry point; it resolves the packaged executable by its full path and
preflights every adjacent FFmpeg/MinGW and Microsoft VC runtime DLL. Do not launch a generated
`target\**` executable directly. Developer `cargo run` and `cargo test` binaries
are routed by `.cargo\config.toml` through `scripts\cargo-runtime-runner.bat`, which prepends the
project-local runtime and reports an incomplete bundle in the terminal instead of opening a chain
of Windows missing-DLL dialogs. Neither path installs DLLs into Windows.

A loader dialog titled `nle_app-<hash>.exe` identifies a generated test harness that bypassed that
runner; it is not the packaged editor and does not mean the listed DLL should be installed. Stop the
raw test process and rerun it through Cargo from this repository. A packaged-editor runtime check
that does not open the GUI is available at the same exact launcher path with `--verify-runtime`.

The monitor decoders share one decoded-frame cache. Its app-wide hard cap
defaults to 1024 MB and can be bounded explicitly:

```powershell
& 'H:\Maelstrom Rust\Launch-Maelstrom-Editor.bat' --cache-mb 512
```

Video cards and timeline clips also have a nested **Proxy Media** menu. It can build, cancel,
enable, disable, retry, or delete an optional 720p editing proxy in the background. Proxies affect
monitor video only; project files, audio, and Quick Export always retain the original. The default
is original media, unavailable proxies fall back automatically, and the disposable cache is capped
at 64 files / 8 GiB. See [`docs/proxy-media.md`](./docs/proxy-media.md).

The v1.1 plan adds the two-machines model (timeline versus picture), explicit
GPU/RAM targets, stock LGPL shared FFmpeg, and a must-employ checklist. Its
first performance gate is 50,000 timeline bars at 60 fps without FFmpeg linked;
mouse movement must never perform decoding.

Hover the compact title-bar performance readout for bilingual session counters and live pipeline
timing. It shows demux/decode/transfer/scale/packing, viewer upload, compositor CPU and optional GPU,
GPU completion, audio mix, and surface-present call measurements with truthful mean/p95 labels,
maximums, and sample counts. Active video-layer count and selected/resolved preview quality are
included; unavailable GPU instrumentation is never displayed as a zero measurement.

The editor title bar reports retained CPU UI/submit time, rolling p95, constant-time
clip count, visible time range, and native timeline primitive counts. A release-only
50,000-clip CPU layout/culling harness is available with
`cargo test -p nle-ui-core --test timeline_performance --release -- --ignored --nocapture`.
The renderer gate executes native shaders on a real adapter, while Windows packaging records
120 CPU frame/surface-submit intervals and rejects CPU or submission-cadence regressions. This
measurement does not claim compositor scanout or GPU-completion timing.

The opt-in finite Phase 0 scenario matrix exercises reverse scrubbing, rapid editor-state
switching, real offline-media detection/recovery, bounded video-strip eviction, forced shared
decoded-frame eviction across four sources, and cancellation after an absolute-path FFmpeg encoder
has started. It writes local-only JSON evidence without launching the GUI; see
[`docs/phase0-scenarios.md`](./docs/phase0-scenarios.md). This matrix does not replace the roadmap's
longer playback soak or sustained cross-hardware memory-pressure gate.

The editor has capped inverse-operation undo/redo (`Ctrl+Z`, `Ctrl+Y`, and
`Ctrl+Shift+Z`), a fuzzy command palette (`Ctrl+P`), pointer/razor/slip/range tools,
hold-`C` temporary razor, pointer-centered wheel zoom, Shift-wheel horizontal pan,
and middle-button or Alt-drag panning. Debug builds expose a live VSync switch beside
the performance HUD. See [ARCHITECTURE.md](./ARCHITECTURE.md) for the hot-path,
worker, cache, persistence, and packaging contract.

The app first presents an undecorated true-black splash with its animated, open
textured cylinder. The English and Japanese embedded PNGs occupy matching
150-degree panels, separated at both ends by equal 30-degree openings. A
background graphics inventory runs while the splash is visible, distinguishing
discrete and integrated adapters and recording an Intel Quick Sync candidate
for the media backend. Adapter presence never substitutes for FFmpeg's per-codec
capability verification. The first GPU frame is presented before project-catalog
and thumbnail disk work or native audio-device negotiation begins; catalog assets
then load on a worker behind the responsive splash. After hardware, catalog, models, and audio
startup resources are ready and the
short minimum presentation has elapsed, the splash waits for a left click; it
never advances automatically. `Esc` closes dialogs (and exits from the splash);
closing the window exits.

Packaged models live beside the executable under `models/manifest.json`; development builds can
point `MAELSTROM_MODEL_DIR` at another manifest directory. Version 1 entries contain a stable `id`,
a safe relative `file`, and optional `expected_bytes`. The loader rejects duplicates, path escapes,
missing files, and size mismatches without discarding valid peers. The repository manifest is
intentionally empty until concrete model artifacts and their consuming engines are selected. The
model schema, provenance requirements, and current code use are documented in
[assets/models/README.md](./assets/models/README.md).

Optional NVIDIA RTX VSR binaries are also excluded from public Git history. Local builds can provide
an entitled runtime through `KRAKEN_RTX_VSR_DIR` or an ignored `crates/nle-upscale/rtx-vsr`
directory; private packages may place `rtx-vsr` beside `Maelstrom.exe`. Exact DLL families, official
sources, loader behavior, restoration paths, and verification are documented in
[docs/OPTIONAL_RUNTIME_ASSETS.md](./docs/OPTIONAL_RUNTIME_ASSETS.md).

The Project Hub is bilingual (English/Japanese), has local Library and built-in
Templates views, grid/list modes, search, sort, thumbnail scaling, in-memory
collections, and New Project state. The selected language is carried by every
New/Open action for the future editor. `MAELSTROM_DEMO_HUB=1` exposes a
debug-only sample project catalog. Normal runs do not fabricate recent projects.
Project cards created or opened in a normal run are saved to the local app-data
catalog by a coalescing background writer. Each project uses a versioned `.nleproj` JSON document with project
timebase, frame rate, resolution, relative and absolute media references. Writes
use a per-project coalescing background worker, atomic replacement, and backup recovery;
returning to the Project Hub does not wait for disk I/O;
autosave restores imported media, tracks, clips, links, fades, gain, playhead,
workspace sizing, and worker-probed source duration. Analysis completion schedules that
duration directly even when a source has no video thumbnail. The first decoded video supplies a
persistent project-card thumbnail, with the selected-language splash art as the empty-project fallback.
Open and Import accept portable project files without blocking the UI. Export
rewrites media references for the destination folder, and Duplicate creates a
separate local project document. Moving a project folder containing its relative
media preserves those links; unavailable relative paths fall back to the saved
absolute source path. Only media used on the restored timeline is analyzed.

Creating a project, or opening a known local project, changes the same window
into a maximized native editor while preserving the system title bar and
taskbar. The first editor slice provides bilingual
Media Pool chrome, background native file selection, OS drag-and-drop import,
pending-probe metadata, and a painter-rendered timeline with three video and
three audio tracks. The first drop into an empty timeline starts at zero. Video
placement creates linked V1/A1 sections that move together, while audio gain and
video/audio fade envelopes remain independent. Track context menus add tracks;
full-width track dividers resize individual rows to reveal thumbnails and waveforms;
gain lines, fade lengths, fade curves, and razor splits are interactive. Each track
has a persistent mute control; muted video layers leave the monitor transparent to
lower layers. Every active unmuted audio track is sample-aligned and mixed into the
single native output; muted tracks are excluded. Audible gain, fade length, and fade
curve follow each independent timeline envelope.
Clip right-click menus are grouped by intent: Open, Edit, Clip, Video, and Audio. Their actions are
the same durable operations used by the Inspector and Effects browser, including enable/disable,
linked selection, video effects, the full nested transition catalog, and equal-power audio
crossfades. Disabled clips remain in place for timing and editing, are visibly hatched on the
timeline, contribute no picture or audio in preview and export, leave lower video layers visible,
and can be restored with normal undo/redo. Legacy projects load their clips enabled.
The Windows package gate also requires the rendered Media Pool card to place that linked pair
through the production timeline drag geometry.
FFmpeg probes placed media and produces bounded waveform peaks and timeline
thumbnails on background workers; quiet waveforms are visually normalized
without changing audio gain. Viewer frames are never sourced from those derived
assets. The inspector lists every FFmpeg stream with codec, dimensions or audio
format, timing, time base, and bit rate. After monitor playback begins it also shows
the actual software, Intel Quick Sync, NVIDIA, D3D11VA, or DXVA2 decoder that produced the frame.
It can force software decoding for compatibility testing. Missing or unreadable media
stays editable and is marked as a magenta offline bar instead of crashing or remaining
in a misleading analysis-pending state.
Active scrubbing continuously publishes the newest desired source tick
to an in-process libav decoder, so the UI thread never performs file I/O or waits
on decoding. Moving the
zoom handles apart widens the visible time range; moving them together narrows
it. The monitor decodes bounded RGBA frames asynchronously with persistent
per-source libav contexts,
supports continuous timeline scrubbing with a draggable ruler handle, centered transport controls,
project-rate frame stepping and timecode (including rational rates such as 30000/1001),
spacebar play/pause, and a video-decoder-independent playback clock. During audible playback the
native device callback is the A/V master; late PCM is skipped to its consumed-sample position. Playback uses
the exact reduced `avg_frame_rate` ratio reported by FFprobe when coalescing constant-rate monitor
requests, so NTSC-style rates do not drift through a rounded decimal rate. Media without trustworthy
rate metadata keeps its exact nonnegative source timestamp and receives exact-only scrub-cache reuse;
Maelstrom does not invent a fallback frame grid. A bounded, cancellable decoded-frame timestamp
index supplies exact local VFR spans off the UI thread; irregular and reordered B-frame fixtures
exercise its addressing contract. The background scan uses one decoder thread to limit contention.
Export retains decoder preroll and floor-samples the same irregular source frames before converting
to project rate; a real trim/slip identity gate covers 30 and 30000/1001. Broader real-media and
cross-backend qualification remains open.
The decoder keeps
the same sticky decoder path for playback and paused seeks: nearby forward
targets decode sequentially, while backward or distant targets seek to a prior
keyframe, flush, and preroll. Latest-target coalescing drops superseded queued
requests. During sustained scrubbing, same-source preroll publishes monotonic
intermediate frames while continuing toward the newest target; a separate
generation rejects output after a gap, media switch, project switch, or cancel.
The decoder worker—not the UI thread—owns a byte-bounded LRU of monitor-sized
RGBA frames. It retains sparse scrub anchors plus the exact latest target, so
repeat and reverse seeks can reuse pixels without allowing a long scrub to grow
memory without limit.
The viewer holds the newest completed frame while the decoder catches up.
When video clips overlap, the viewer now resolves up to four visible, unmuted tracks and
composites their retained frames bottom-to-top. Each layer has an independent latest-wins decoder
and texture slot, so a slow or unreadable upper source does not block a ready lower source.
The bilingual Inspector provides a resettable transform workflow for Fit, Fill, Stretch, and
Original Pixels sizing, opacity, linked or independent scale, position, clockwise rotation, anchor
point, four-edge crop, and horizontal/vertical flip. These durable values apply independently to
every layer, survive save/reopen, and participate in bounded undo/redo. A shared CPU-neutral
composition plan produces the same project-space geometry for preview and export. The packaged
viewer lowers that plan into one retained native GPU callback: four fixed input slots are reused,
two canvas-sized outputs alternate front/back, and unchanged frames reuse the last composition
without rebuilding pipelines, textures, or UI image meshes. Decoder letterboxing and sRGB input
interpretation remain explicit, while a deterministic egui fallback keeps headless tests and
recovery behavior intact. Quick Export consumes that plan for up to four visible video tracks, including the
full transform, crop, sizing, flip, opacity, and shaped video-fade contract over a black project
canvas. Its background audio graph mixes every audible track with mute/solo, clip and track gain,
channel trim, pan, timing, and shaped fades. Still images use a five-second default clip, bounded
off-thread thumbnails, a frozen monitor decode address, and the same transform/effect/fade/export
path as video while remaining freely trimmable. Active audio effects and video stacks above four
layers remain visibly gated instead of being silently omitted. Completed renders
are promoted from a same-directory staged file, so probing, graph, encoder, or cancellation failure
cannot delete an existing destination. The total
configured monitor-cache budget remains fixed and is divided across the four slots.
The Playback menu and Viewer header offer separate moving and paused Auto, Full, Half, Quarter, and
Eighth preview controls without changing export resolution. Full is the default and decodes the
entire physical viewer raster, including Windows display scaling; it is no longer silently capped
at 1280×720. High Quality Playback defaults on and uses bicubic scaling. Auto is opt-in, measures
completed decoder-request turnaround against the project frame budget, downshifts only after
sustained pressure, and requires a longer stable recovery before raising quality. Its label always
shows the fraction currently in use. Manual choices are saved with the project, and any resolution
change obsoletes old decode requests while the last good frame remains visible until its replacement
arrives.
Adjacent video clips can carry native centered Cross Dissolve and Dip to Black operations without
weakening the timeline's sorted, non-overlapping clip model. The timeline gives each type a distinct
visual and the bilingual Inspector adds, removes, and edits duration and curve with bounded undo/redo.
Cross Dissolve requires real unused source frames on both sides of the cut and assigns independent
preview slots to continuously advancing outgoing and incoming frames. Dip to Black needs no trimmed
source handles: it fades the visible outgoing frame to project black at the cut, then raises the
incoming frame over an opaque black matte at that track's compositing depth, so lower tracks cannot
show through. Quick Export lowers the matching raw quadratic envelopes and validates the same
centered windows. Adjacent audio clips can independently carry native equal-power crossfades at
either edge. The Inspector limits their centered duration to real unused source audio, the live
mixer keeps two decoder lanes for the same track only while their windows overlap, and sample-level
sine/cosine gains preserve energy through the cut. Entering or leaving that overlap preserves the
running device clock and the retained lane's queued audio. Quick Export expands the same source handles,
uses matching quarter-sine envelopes, and inserts timeline silence by exact 48 kHz sample count.
Existing v1–v6 documents migrate losslessly to the v7 audio-transition schema.
Selected video clips expose a compact professional Basic Correction group. Highlights and Shadows
shape broad tonal regions; Whites and Blacks target the narrow ends of the range with normalized
`-100%..100%` controls, where zero is identity. Every control can be scrubbed, reset, and animated,
and the native viewer and Quick Export use the same clamped tonal masks. Colored diamond keys map
through source time, so trims and slips do
not rewrite animation. Clicking a key seeks to it; dragging retimes it on project-frame boundaries
without jumping from an off-center grab, overwriting a neighboring key, or creating more than one
undo step. Only the selected clip's active correction is scanned and all visible diamonds share one
mesh, preserving the timeline's bounded drawing path.
Native titles live in their own lane beneath the timeline ruler, so they remain fast overlays rather
than consuming decoder slots. A title can be created at the playhead, moved or trimmed directly,
and edited in the bilingual Inspector for text, alignment, size, fill, outline, shadow, position,
opacity, fades, and visibility. Titles use the bundled Noto Sans JP font through one deterministic
RGBA raster path for both the live viewer and Quick Export; their timing and styling survive
save/reopen and bounded undo/redo; titles introduced the v4 format and remain intact in v7.
On supported H.264/HEVC/AV1 media, the live monitor prefers NVIDIA CUVID or
Intel Quick Sync, then tries Windows D3D11VA and legacy DXVA2 so AMD and other
DirectX-capable adapters are accelerated too. Opaque DirectX frames are
transferred to CPU memory only for the retained monitor-texture update. Any
hardware open or runtime decode failure retains a bounded software session.
Exact reverse-scrub frames remain visible
while a newer reverse target is retained, avoiding the cancel-on-every-mouse-
move starvation that affects long-GOP footage. Native audio playback uses
in-process libav decode/resampling and a bounded, lane-aware CPAL device ring. One
long-lived decode worker owns a coalescing latest-seek slot and sticky per-track
sessions, so repeated timeline seeks cannot accumulate decoder threads. Play, pause,
timeline seeks, overlapping tracks, clip gain, and shaped fade envelopes synchronize
without launching an FFmpeg subprocess; the analysis meters display final mixed
left/right peaks actually consumed by the output callback.

Quick Export takes an immutable timeline snapshot and encodes H.264 video plus
AAC audio on a cancellable worker while editing continues. It preserves black and
silent gaps, clip trims, video fades, and independently edited audio source trims,
gain, fades, and equal-power crossfades. Encoder selection follows the detected hardware profile:
Quick Sync, NVENC, or AMF, then Media Foundation and LGPL OpenH264 fallbacks.
Cancellation terminates and joins the worker process and removes partial output.
The Licenses control opens the bundled third-party notices inside the editor.

For a self-contained Windows package, first build the pinned FFmpeg 8.1 LGPL shared
runtime in WSL2, then package against it:

```powershell
.\scripts\build-ffmpeg-lgpl-windows.ps1
.\scripts\package-windows.ps1 -FfmpegBundleRoot .\.deps\ffmpeg-project-8.1 -SkipSmoke
```

`-SkipSmoke` builds and assembles the package and checks required adjacent runtime files without
launching the editor. It preserves existing `dist\last-*-smoke.json` reports as historical evidence;
they do not qualify the new executable. `PACKAGE-STATUS.json` inside the new package records its
executable SHA-256 and `smoke_status: not_run`. The same file records `passed` only after the default
packaging smoke completes successfully; this is not a full release qualification.

Repository agents must use `-SkipSmoke`: the default packaging smoke launches the editor directly.
An agent may open the editor only after an explicit user request, through
`H:\Maelstrom Rust\Launch-Maelstrom-Editor.bat`. The launcher's `--verify-runtime` option checks
required files without opening a window.

Packaging copies `vcruntime140.dll` app-local from either a trusted, operator-supplied authorized
AMD64 directory supplied with
`-VcRedistCrtDirectory` or the newest auto-discovered installed Visual Studio x64
`Microsoft.VC*.CRT` Redist directory. It never takes that DLL from `System32` or a download.

The WSL build requires `mingw-w64 make cmake ninja-build nasm pkg-config git ca-certificates llvm-19`.
It pins FFmpeg, nv-codec-headers, and Intel oneVPL by commit, emits MSVC import libraries,
records hashes for every runtime and link artifact, and enables NVDEC/NVENC,
D3D11VA/DXVA2, Quick Sync, and Media Foundation without GPL/nonfree components. The package
step verifies that manifest and every hash before building Maelstrom. The separate
`fetch-ffmpeg-lgpl.ps1` helper remains a development-only convenience.

Workspace tests that link FFmpeg need the project bundle and the directory containing
`libclang.dll` explicitly selected:

```powershell
$env:FFMPEG_DIR=(Resolve-Path '.deps\ffmpeg-project-8.1').Path
$env:LIBCLANG_PATH='<directory containing libclang.dll>'
$env:PATH="$env:FFMPEG_DIR\bin;$env:LIBCLANG_PATH;$env:PATH"
cargo test --workspace
```

The package is written to `dist\Maelstrom-Windows-x64`. Without `-SkipSmoke`, it is smoke-tested with
only its adjacent DLLs available. That smoke creates a deterministic A/V clip and requires the
packaged app to produce linked bars, metadata, a nonempty waveform, a decoded monitor frame,
advancing playback, live audio meters, confirmed FFmpeg export progress, and a cleanly cancelled
snapshot export with no orphaned process or partial output. Its startup-presentation,
120-frame CPU/surface-submission, and real-media acceptance reports are retained as
`dist\last-startup-smoke.json`, `dist\last-surface-submission-smoke.json`, and
`dist\last-media-acceptance-smoke.json`, outside the shipped bundle. The surface report includes
the machine, renderer/driver, observed decoder and active encoder, preview/cache, and display
context described in [`docs/performance-reports.md`](./docs/performance-reports.md). Packaging
rejects a
first successful surface presentation of one second or slower. The shipped folder
also includes FFmpeg's LGPL text, the exact build manifest, and the checksum inventory.

Historical macOS build and packaging scripts remain in the repository for reference only. They
are unsupported, are not release-gated, and do not contribute to product completion. New platform
work should target Windows; Linux portability may be considered later.

The app owns the splash lifetime and starts Hub resources, hardware detection, and audio-device
negotiation only after its first successful surface presentation. Splash art, the bilingual font,
and native window branding are embedded from `assets/splash`, `assets/fonts`, and
`assets/branding`. The original 1,254 px logo remains the source artwork while the window uses the
purpose-sized `maelstrom-window-icon.png`; no asset files are read at runtime.

- Foundation plan: [RUST_NLE_FOUNDATION_PLAN.md](./RUST_NLE_FOUNDATION_PLAN.md)
