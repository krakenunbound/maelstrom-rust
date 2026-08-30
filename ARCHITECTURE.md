# Maelstrom Foundation Architecture

This file is the implementation contract for the Blick-class foundation. Performance is an
architectural property: a perfect monitor is not allowed to make the timeline wait.

## Sacred UI thread

The UI thread performs only input sampling, `EditorState` mutation, immediate-mode layout,
timeline primitive generation, GPU upload/submission, and presentation. Frame-loop functions are
marked `HOT PATH — no IO` where appropriate.

The UI thread must never:

- call FFmpeg, decode, encode, scan media bytes, or perform project filesystem work;
- wait on a lock held by a media worker;
- allocate a widget or heap object per timeline clip;
- clone media paths or large strings per frame;
- walk every project clip to draw a small visible interval.

The editor has two cooperating machines:

1. The UI/timeline machine mutates authoritative timeline data and paints cheap bars.
2. The media machine probes, decodes, analyzes, plays audio, saves, and exports on workers.

They exchange bounded commands and results. A worker may lag or be cancelled; the UI may retain a
stale frame or black monitor, but interaction continues.

## Workspace ownership

- `nle-timeline`: media-independent timeline source of truth, edit kernel, inverse-operation
  history, sorted arrays, visibility cache, and banding. It does not depend on UI, GPU, or decode.
- `nle-compositor`: immutable, fixed-capacity project-space composition requests and deterministic
  crop, sizing, anchor, scale, rotation, position, flip, and opacity geometry. It depends only on
  `nle-timeline`, so preview and future export lowering can share one transform contract.
- `nle-ui-core`: immediate editor/hub state and GPU-neutral timeline primitives. The timeline is a
  single custom canvas, not a widget tree.
- `nle-render`: retained `wgpu` pipelines and reused instance buffers for solid and textured
  rectangles.
- `nle-decode`: sticky FFmpeg monitor sessions, latest-request coalescing, cancellation, preroll,
  hardware preference with software fallback, and a bounded byte LRU.
- `nle-waveform`: cancellable probe, waveform, thumbnail-strip, and duration analysis.
- `nle-audio`: independent CPAL output, worker decoding, coalesced seek commands, bounded
  per-track PCM lanes, sample-aligned mixing, shaped gain/fades, and live mixed-output meters.
- `nle-project-io`: versioned portable documents, relative-path resolution, and atomic writes.
- `nle-title`: deterministic CPU title layout and RGBA rasterization against the bundled Noto Sans
  JP font. Preview and export consume the same bounded title plate and fade contract.
- `nle-export`: snapshot-at-start background H.264/AAC export with cancellation and encoder
  fallback.
- `nle-app`: native window/event ownership, orchestration, worker result polling, and GPU upload.

Dependency direction is inward toward data. In particular, `nle-timeline` must remain independent
of FFmpeg, `wgpu`, and `egui`.

## Timeline and editing

Tracks own contiguous clips sorted by start time. Clips hold numeric media IDs rather than paths or
frames. A derived structure-of-arrays cache stores starts, ends, IDs, and indices. Visibility uses
binary search; zoomed-out output bands adjacent bars to a viewport-sized primitive count. Scratch
vectors and GPU buffers retain capacity across frames.

The editor media catalog is a compact slot array: media ID `n` occupies slot `n - 1`. New imports
append IDs and foundation projects do not delete catalog entries. Restore canonicalizes harmlessly
reordered JSON by ID and rejects sparse IDs transactionally before playback, analysis, thumbnails,
or timeline labels can resolve the wrong path. A derived draw-slot array retains each media's
waveform Arc, thumbnail atlas handle, offline/failure bits, and flag color. Worker results and flag
edits invalidate that array; the visible-clip loop performs one array lookup rather than media
`HashMap` lookups or a flag scan for every clip.

The foundation edit kernel owns move, trim, razor, slip, roll, insert/overwrite, replace, delete,
linked A/V selection, gain, fades, and mute. Successful edits bump generation once. Undo/redo stores
capped batches of inverse clip/track operations (256 entries), not project snapshots. Persistent
project snapshots are used at background save/export boundaries and temporarily before gestures.
Clip arrays contain shared immutable records (`Clip` over `Arc<ClipData>`). Snapshot capture copies
lightweight handles; any mutable field access detaches only that clip through copy-on-write.
Nested audio/video effects and keyframes remain isolated from undo and export snapshots. Project
JSON is unchanged; Rust constructors use `Clip::new(ClipData { ... })`.

History comparison has a linear fast path without per-clip maps when track and clip cardinality are
unchanged. It skips shared records by identity, compares other records by value, and records only changes, including a
single relocated clip; inserts, deletes, cross-track moves, and complex reorders fall back to the
general structural diff. The release 50,000-clip gate requires history recording to remain below
the same 2 ms interaction budget. The editor records against the live timeline after a gesture,
avoiding a redundant full after-state clone while retaining the before snapshot required for undo.

Ordinary relocation uses a binary destination search and in-place rotation of only the crossed
clip range, independently for linked audio/video tracks. Only affected location-index entries are
updated; multi-change reorders retain stable sorting on the affected track. Collision validation
checks the destination's unchanged neighbors and the final intervals of other edited clips before
any mutation. Ten local release trials passed both unchanged 2 ms history limits after shared-record
capture: press p95 0.3481 ms and edit/release p95 0.9921 ms. These CPU measurements do not establish
UI-present or cross-hardware latency; see `docs/timeline-relocation-performance.md` for evidence.
Per-clip allocation and indirection increase base storage overhead; retained draw caches still use
compact columns. Canonical restore retains shared clips, but validates/normalizes cloned nonempty
video-effect vectors before deciding whether a record must detach.

Timeline-bound Media Pool and operating-system file drops use non-ripple overwrite edits on V1/A1
or A1: occupied sections become source-accurate outer tails and later clips do not move. The explicit
Add action remains append-only. Placement emits background analysis only after the bar exists, and
the complete overwrite is one undoable transaction.

New and reset workspaces reserve 66 percent of the available editor height for the timeline,
making it the dominant vertical work area while leaving a compact viewer above. The responsive
default follows later window-size changes; the first splitter drag transfers ownership to an exact,
durable user preference. Temporary viewport clamping never overwrites that preference. Pre-marker
projects migrate the former exact 330, 520, 640, and 760 logical-pixel defaults while retaining
non-default custom values. Panel splits remain draggable and durable.

The first successful media placement replaces the empty project's legacy 4:12 time range with a
bounded full-extent view, making thumbnails, waveforms, fades, and drag targets immediately usable.
An unprobed source appears immediately with a 15-second placeholder. The project snapshot records
which exact clip IDs still grant the probe ownership of that placeholder plus the worker-produced
duration scalar; waveform and thumbnail data remain runtime caches. A probe may extend only those
untouched IDs, stops at the next occupied section, and refits the first-placement view. Razor,
trim, slip, replace, overwrite, or another source-timing edit relinquishes that ownership, so a
late result cannot undo user work. The same capped undo transaction stores before/after ownership;
undoing a cut, delete, overwrite, or placement restores the correct probe rights, and redo of a
now-analyzed placement immediately reapplies the durable known duration. Legacy projects migrate
the exact placeholder shape, while new
snapshots distinguish a deliberately retained 15-second edit from unresolved analysis. Later
placements and manual navigation preserve an explicitly chosen pan/zoom. The package gate records
the post-probe production view and rejects both an unfitted view and failure to reconcile its
deterministic 60-second source.

Timeline positions remain integer microsecond ticks. The project document's rational frame rate is
a playback/display property that drives frame stepping, timecode, dynamic trim boundaries, end-frame
hold, monitor request quantization, and audio discontinuity tolerance; it is not duplicated in the
editor snapshot.

## Decode, playback, and caches

Scrubbing publishes the newest desired media tick. `nle-decode` keeps sticky per-media FFmpeg
contexts, coalesces superseded targets, cancels obsolete work, seeks backward to a keyframe for
backward/far jumps, and decodes forward to the requested frame. It never requires a complete frame
index before playback. The inspector reports the decoder backend that actually produced a frame;
users can force software decoding.

The monitor cache is byte-bounded (`--cache-mb`, clamped to 512–2048 MiB), sparse during scrubbing,
and invalidated by project/source generations. It is a live acceleration cache, never the source of
truth for an edit. Waveforms and thumbnail strips are derived display caches. Analysis starts only
after media is placed on the timeline. Missing/unreadable media remains editable as a magenta
offline bar.

User proxy media is a separate derived-video path owned by `nle-proxy`. A cancellable worker uses
the packaged LGPL FFmpeg to create a video-only, intra-frame, timestamp-preserving 720p MPEG-4 file,
publishes it atomically, and prunes the disposable local-app-data cache to 64 files / 8 GiB. The app
keeps the proxy record and enable choice in runtime state only. It substitutes that path solely at
monitor `DecodeRequest` construction; audio targets, media analysis, project snapshots, and export
plans always retain the original source. A distinct monitor-cache epoch advances on every routing
change so original and proxy pixels cannot share one cache identity. Completion revalidates the
source fingerprint, explicit enable rechecks the proxy file, and post-generation pruning is
reconciled against retained records. If an enabled proxy is deleted or cannot decode, the decoder
error boundary disables it and resubmits the original without adding disk access to the monitor hot
path. Regeneration and deletion perform replacement/removal on owned workers; failed removal keeps
a disabled cleanup record so Retry/Delete cannot rediscover a bad file as Ready. See
`docs/proxy-media.md`.

Live preview resolves at most four visible, unmuted video tracks into a fixed bottom-to-top
target array. Each slot owns an independent latest-wins monitor decoder, generation, retained frame,
and native texture. Its single worker keeps only the current source's FFmpeg session sticky, and the
configured monitor-cache byte cap is divided between the four slots rather than quadrupled. A late,
missing, or invalid layer is held or transparent without blocking the other
slots or timeline input. Each ready texture is lowered through the allocation-free
`nle-compositor` plan, which applies crop, Fit/Fill/Stretch/Original sizing, anchor-relative scale,
clockwise rotation, normalized position, flip, fade, and opacity in project-pixel space. UI-core maps
that quad into a GPU-neutral viewer canvas and remaps content UVs around decoder letterboxing. The
packaged app uploads each newest decoded frame into one of four fixed, reusable sRGB texture slots.
A retained native callback composites only when input, geometry, or canvas size changes, using two
canvas-sized outputs that alternate front/back. Pipelines and same-size input textures are reused;
stable UI frames blit the current output without recompositing or uploading decoded pixels through
egui. Project resolution, including portrait rasters, determines viewer aspect and geometry while
output allocation follows the physical viewer size. The deterministic egui mesh path remains as a
headless/device fallback. Quick Export lowers the same composition plan into a bounded
FFmpeg graph for up to four unmuted video tracks. Source dimensions are probed and cached on the
export worker; crop, sizing, scale, anchor, position, rotation, flips, opacity, and the shared
quadratic/gamma video-fade envelope are applied before bottom-to-top overlay on project black.
Video transitions remain separate durable typed operations bound to exact adjacent cuts, so clips
stay ordered and non-overlapping. Cross Dissolve derives two temporary source ranges: the editor
validates saved handles, preview assigns independent bounded decoder slots, and export expands the
same seek/trim ranges. The outgoing frame remains the base while the incoming frame uses the shared
raw quadratic envelope. Dip to Black keeps both clips inside their normal trims and inserts a
full-project black matte at the transition track's compositing depth: the matte rises over the
outgoing half and remains opaque beneath the incoming half, which fades up without another decoder
slot. Structural
edits prune invalid or overlapping operations; v1–v5 documents migrate idempotently to the v6 typed
transition schema, with legacy transitions defaulting to Cross Dissolve.
Each clip also owns a durable `enabled` flag, defaulted to true during legacy deserialization.
Disabling a linked clip pair is one atomic timeline mutation. Disabled placements still contribute
their authored end time, preserving project duration and gaps, but are excluded before preview
decoder/audio target selection and before export probing, input construction, and filter planning.
Transitions or crossfades touching a disabled clip are bypassed until both sides are enabled again.
Audible audio tracks are independently trimmed, timed, gain/pan/channel-adjusted, curve-faded, and
mixed. Equal-power audio crossfades are separate durable operations on exact adjacent same-track
cuts. The editor validates saved pre/post handles, emits outgoing and incoming targets with a shared
centered window, and keys decoder sessions by track and clip so two sources on one track can coexist
without collapsing their state. Lane-set reconciliation retains queued PCM and the global device
clock at both window boundaries; newly joining lanes use their own device-frame origin. The device
callback evaluates cosine/sine gain per consumed sample.
Export expands the identical handles, shifts ordinary clip fades back to their authored clip time,
lowers quarter-sine envelopes, and aligns sources with an integer 48 kHz silence delay instead of
depending on mixer timestamp behavior. Structural edits prune invalid or overlapping audio
transitions; v1–v6 documents migrate idempotently to the v7 schema. More than four video tracks,
still-image clips, or active unmapped audio effects stay
disabled in the UI and are also rejected by the worker, so unsupported state cannot silently
disappear from the output.

Every viewer update is first described by immutable, allocation-free preview metadata: sequence
generation, playhead, selected and resolved quality, output size, and ordered source/priority
slots. Full uses the quantized physical viewer raster (logical bounds multiplied by display scale),
with only an 8K allocation guard; half, quarter, and eighth are exact divisors of that raster.
Auto observes latest-request decoder turnaround against a frame-rate-derived budget, uses sustained
breach and longer recovery windows to avoid oscillation, and changes only runtime resolution.
Manual quality is durable view state; Auto's current resolution is runtime-only. Output-size changes
cancel and generation-obsolete prior layer requests but retain the last good texture for continuity.
For indexed VFR video, trim and slip continue to change the canonical `source_in`; playback adds the
clip-local timeline offset and floors that logical source time to the greatest retained packet PTS.
At the exact held timeline endpoint, the source out-point remains exclusive: the index resolves from
the final representable source microtick before it, independent of the project frame grid. Empty
indexes and CFR media retain rational project/source-rate behavior. Forward and decreasing preview
requests carry the same resolved boundary and adjacent local span into decoder/cache policy.
Before decode admission, the app releases every positional slot that no longer contributes. It then
orders contributing video slots in a fixed four-entry array by declared priority, with the visually
topmost layer winning ties, while preserving each result's positional compositor slot. This gives a
new top visible source first access to bounded source/session capacity without allocating on the
submission path. Audio scheduling remains independent and includes every audible lane; this policy
does not yet claim priority-driven preemptive eviction of another actively contributing source.

Playback advances independently of video decode. Late video frames are held or skipped rather than
stalling the clock. During audible playback, the CPAL device callback owns the shared A/V clock and
the video playhead maps to its consumed-sample position. Audio has its own worker and device stream;
an underrun emits device silence and late decoded PCM is discarded to catch up instead of blocking
the UI or replaying stale sound.

The first successful splash presentation is an explicit startup boundary. The renderer decodes each
embedded splash image once, uploads it for the cylinder, and retains that same RGBA allocation for
the later Project Hub backdrop. Hub texture installation, hardware detection, catalog/thumbnail
loading, model preloading, and audio-device negotiation start only after that boundary. The model
registry reads a versioned package manifest on the startup-resource worker, validates safe relative
paths and optional byte counts, and retains successful files as shared immutable byte buffers for
future inference engines. One invalid entry does not discard valid peers; errors surface in the
Project Hub. The native window uses a purpose-sized embedded icon while the full-resolution
branding source remains available as artwork.

## Invalidation and persistence

Timeline structural caches rebuild only when structural generation changes. Runtime frames,
waveforms, decoder identities, error states, and GPU handles never enter project documents.
Autosave observes durable generation, clones only at a persistence boundary, and performs atomic
temporary-file replacement on its writer thread. Pending documents coalesce independently by
project path, so returning to the Hub queues the latest state without waiting for filesystem work
or allowing a quick project switch to discard another project's save. Project-catalog mutations
use a separate coalescing writer; catalog and thumbnail reads run on the startup-resource worker.
Project media stores absolute and project-relative paths; moving a portable project can resolve the
relative form. Media-analysis completion schedules durable duration changes directly, including
audio-only sources and failed thumbnail extraction, rather than relying on a later redraw.

Export receives an immutable project snapshot at start. The worker owns source probing, graph
lowering, FFmpeg execution, encoder retry, and progress; the interaction thread owns none of that
work. Editing remains live while the worker renders, and cancellation removes the partial output
and temporary filter graph without leaving an orphaned process or worker. FFmpeg writes a unique
same-directory staged output; only a successful render crosses the atomic replacement boundary, so
preflight or encoder failure preserves any existing destination file.

Basic color correction is an ordered, animatable timeline operation. The native WGSL path and the
FFmpeg export lowering share encoded-sRGB math: Highlights/Shadows use quadratic masks and
Whites/Blacks use narrower eighth-power masks over clamped tonal luma. Zero is identity and legacy
documents default newly introduced controls to zero.

## Rendering, diagnostics, and legal boundary

One `wgpu` device/queue draws the app. The timeline emits batched native rectangles/textures and the
monitor is one retained texture. Clip labels and waveform-status layouts are retained across frames
and omitted when bars are too small to read; flags are fixed rectangle primitives, so the visible
clip loop does not allocate text or polygon data. A fixed integer hash of media ID selects a bounded
dark palette pair: linked bars share source identity while video remains blue-family, audio remains
green-family, and offline media stays unambiguously magenta. Debug builds expose the CPU frame HUD
and a VSync toggle. Tracing records the renderer adapter and each sticky decoder backend, never one
event per rectangle.

The retained title-bar label stays compact at the window edge. Hovering it opens English/Japanese
session diagnostics for monitor requests, completed/presented frames, rejected stale frames,
late completions that held a prior frame, decode errors, p95 turnaround, native/fallback uploads,
and audio underruns. Monitor timing uses a fixed 120-sample ring and is summarized periodically;
it does not log or allocate per frame. The native audio callback only increments shared atomics for
lock misses and underrun device frames, while late decoded-frame discards are counted on the worker.
These counters are runtime-only and never enter project persistence.

The same hover contains a compact bilingual live-pipeline table. It reuses the bounded decoder,
viewer, compositor, GPU-completion, native-audio, and surface-present timing accumulators already
owned by their responsible layers; it adds no per-frame logging. Decoder and audio stages show
mean/maximum CPU time, bounded viewer/GPU windows show p95/maximum, and every observed row shows its
sample count. Active contributing video layers and selected-to-resolved preview quality provide the
measurement context. Unsupported GPU timestamps and stages without samples remain `unavailable`,
not zero. Reading compositor callback timing uses `try_lock`; a busy render callback therefore
skips that HUD sample instead of blocking the UI thread.

The existing 120-frame surface-submission report is schema-versioned and carries the measurement
environment with its timing window: actual renderer adapter/driver data, every decoder backend that
produced a monitor frame, the most recently started encoder in the export fallback chain, CPU/RAM,
selected and resolved preview quality plus requested decode size, cache cap, and display refresh
when the platform reports it. Full package smoke publication waits for observed decoder and encoder
evidence; a cadence-only run explicitly reports unobserved media backends. Unavailable platform
facts serialize as JSON `null`, never a plausible-looking zero or `"Unknown"` value.

The same report also carries fixed atomic aggregates from the bounded monitor decoder lanes:
cache lookup, demux packet retrieval, decoder send/receive/flush, hardware-to-CPU transfer, scaler,
RGBA copy plus letterbox, and whole worker request. Each has sample count, total/mean/max CPU
milliseconds. Software decode has zero hardware-transfer samples by design. These spans do not
claim GPU upload/compositing completion or scanout. A separate audio timing sub-object reports the
whole output-callback CPU boundary and a successful-lock mix/render CPU boundary. The latter
includes lane mix/fades/effects, output conversion, meter accumulation/store, device-clock
advancement, and underrun bookkeeping; lock-failure fallback callbacks do not produce a mix/render
sample, while acquired paused callbacks count. Neither boundary claims device/DAC latency.
Both audio fields are monotonic elapsed-time measurements around CPU-side work, not per-thread CPU
accounting, so scheduler preemption may be included.

Its viewer timing sub-object records CPU/API submission only: successful native RGBA uploads,
changed-composition command encoding, and the `frame.present()` call handoff. All three use bounded
120-sample windows and are deliberately separate from GPU execution, GPU completion, and scanout;
the package full-media smoke waits for at least one successful upload and one actual composition
encode before publishing this evidence.

Schema 6 adds a separate GPU submission-completion sub-object. During the opt-in editor report only,
`nle-render` keeps one completion callback in flight and uses non-blocking device polling; a stalled
submission skips later samples instead of allocating an unbounded callback queue. Its fixed
120-sample p95/maximum window measures CPU monotonic elapsed time from immediately before queue
submission until wgpu reports all GPU work through that submission complete. The value can include
earlier queue backlog and driver scheduling, so it is not an isolated GPU-pass duration. It also
includes callback dispatch/non-blocking poll observation delay, ends before the presentation
handoff, and cannot prove DWM composition or physical scanout.

Schema 6 also snapshots the existing cumulative runtime counters for monitor request/completion/
presentation/drop/hold/late/error outcomes, native/fallback viewer uploads, and audio underrun,
callback-lock, and late-discard faults. This snapshot covers process/session lifetime rather than
the fixed 120-frame timing window and adds no per-frame logging or persistent project state.

The opt-in Phase 0 scenario matrix is a finite integration check, not a steady-state benchmark. It
uses generated media and the existing public decoder/App/export paths to verify latest-wins reverse
scrubbing, alternating editor restoration, missing-file detection followed by decode recovery,
bounded video-strip eviction, forced byte-LRU eviction in the shared decoded-frame cache with four
distinct sources, and cancellation only after an absolute-path FFmpeg process reports its encoder.
Reports are atomically replaced inside an ignored workspace artifact directory. The ten-minute
playback soak and sustained cross-hardware memory-pressure proof remain separate roadmap gates.

Shipping uses the pinned FFmpeg 8.1 LGPL shared bundle. Windows DLLs are packaged beside the
executable. GPL/nonfree codec libraries are rejected. H.264 export prefers available Windows
hardware and then Media Foundation/OpenH264. FFmpeg is linked as libraries for playback/scrubbing.
Bundled `ffmpeg`/`ffprobe`
helpers are used only by cancellable background analysis/export workers, never on the interaction
path.

Monitor decode prefers the platform hardware backend and always retains a software fallback. The
inspector records the backend that really produced the frame rather than the startup adapter guess.

Windows first tries the low-latency codec-specific CUVID and Quick Sync decoders. If neither can
open, the same sticky monitor session negotiates FFmpeg's generic D3D11VA device and then DXVA2.
Both DirectX paths select only a hardware format advertised with `HW_DEVICE_CTX`, transfer the
opaque surface before scaling, and fall back to a retained software session if hardware fails
after opening.

## Required gates

Before a foundation change lands:

- run workspace tests and `clippy -D warnings` with the pinned FFmpeg development environment;
- run the release 50,000-clip wide/detail/playhead harness;
- run real-media decode, backward-seek, cancellation, audio, probe, and export tests when those
  layers change;
- verify Windows packaging rejects GPL/nonfree dependencies and starts using only bundled DLLs;
- gate packaging on 120 CPU frame/surface-submit samples, recording CPU p95, submission-interval
  p95, average submission cadence, and at least one completed measured GPU submission outside the
  shipped bundle; do not describe submission completion as compositor scanout or isolated GPU-pass
  execution timing;
- make the Windows package prove real-media linked bars, metadata, waveform, monitor decode,
  playback, live audio meters, confirmed FFmpeg export progress, cancellation without a partial
  output, and exact process-tree cleanup using only its adjacent runtime;
- stop every test app/helper process before handing control back.

The measurable target is ≤2 ms for the 50k interaction path, ≤8 ms p95 for the CPU frame evidence,
O(visible clips) detailed drawing, bounded caches, and a timeline that never waits for media.
