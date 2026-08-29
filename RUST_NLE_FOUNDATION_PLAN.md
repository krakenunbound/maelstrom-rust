# Rust NLE Foundation Plan (Blick-class performance)

**Document type:** implementation plan for a local coding agent  
**Version:** 1.1 (2026-08-23) — hardware targets, FFmpeg employment (no special fork), must-employ checklist  
**GitHub:** https://github.com/krakenunbound/maelstrom-rust
**Goal:** begin the *foundation* of a native non-linear video editor (NLE) in Rust whose interaction path matches Blick-class feel: near-instant timeline, zero-lag zoom/pan/select/trim, and non-blocking video scrubbing.  
**Non-goal of this document:** ship a Premiere/Resolve clone, plugins, color science, or 1.0 feature parity.  
**Language:** Rust (stable, edition 2024 or 2021 — pick one and freeze it in `Cargo.toml`).  
**OS target for the current foundation:** Windows. Linux is an acceptable secondary milestone after
the Windows editor is polished; macOS is explicitly deferred by product direction.

This plan is written so an agent can start coding *without* re-deriving the architecture. Follow it in order. Do not skip the performance budgets or the “sacred UI thread” rules.

---

## 0. The one sentence that must never be violated

> **The UI / timeline interaction path never waits on media decoding, disk I/O, effects, or compositing.**

If a mouse-move, zoom, pan, box-select, slip, razor, or trim ever calls FFmpeg, waits on a decoder, walks a retained widget tree, or uploads textures per clip, the architecture is already lost.

Blick’s uniqueness is **not** “Odin is faster than Rust.” It is this architecture:

1. Timeline UI = pure structural data + cheap GPU draw of bars.
2. Media = a completely separate, cancelable, cached pipeline.
3. Immediate-mode (or equivalently tight) UI rebuilt every frame from state.
4. Obsessive measurement of the interaction path.

Rust can match this. Idiomatic “safe Rust + egui + ffmpeg crate on the UI thread” will not.

### 0.1 Two machines, one window (why it feels like zero lag)

The thing people call “no latency, no hiccups, even with high resolution and multiple layers” is **two separate systems**:

| Machine | What the user feels | Cost driver | Must stay instant? |
|---------|---------------------|-------------|--------------------|
| **Timeline** | load, zoom, pan, select, trim, playhead drag, 10k–30k clips as bars | clip count (solved with SoA + banding) | **Yes. Always.** Independent of 720p vs 8K. |
| **Picture** | monitor/preview, 1x playback, multi-layer composite | resolution, layer count, codec, GOP size | **Best-effort.** Stale frame allowed. Never block the timeline. |

Most NLEs hitch because they do: scrub → seek decoder → wait → then move the playhead.  
Blick does: move playhead + bars immediately → decode workers catch up → monitor may lag a frame.

**8K and extra layers make decode/composite harder. They are not allowed to hitch zoom, pan, or playhead drag.**

---

## 1. Product definition (foundation scope)

Build a **native desktop NLE kernel**, not a web app.

### Foundation must demonstrate (acceptance demo)

A single binary that:

1. Opens to a blank project instantly (no New Project wizard).
2. Drops one or more video files onto a timeline (or via file dialog).
3. Shows clips as **bars** immediately — even 10,000+ clips without hitch.
4. Zooms, pans, and scrubs the playhead with mouse — UI stays at display refresh (target 120 Hz if the display is 120 Hz).
5. Monitor panel shows actual video frames **asynchronously**. Stale/placeholder frames are allowed; **stalling the timeline is not**.
6. Export can be started and cancelled; the editor remains interactive during export (export can be a stub that writes a dummy file in Phase 0, real encode in Phase 3).
7. Autosave of project JSON to disk on a background thread.

### Explicitly out of scope for foundation (do not build yet)

- Full effects graph, nodes, blending modes, time remap, speed ramps
- Multicam, nested sequences, multiple timelines
- Color management / LUTs / HDR
- Plugin APIs (CLAP, OFX)
- Text titles beyond a placeholder clip type
- Linux packaging, installers, licensing (optional later milestone)
- macOS runtime, packaging, and distribution work
- Cloud, accounts, telemetry
- AI features

If tempted to add any of the above before the interaction path is proven, stop.

---

## 2. Performance budgets (non-negotiable)

Measure these from day one. If a PR blows a budget, it does not land.

| Path | Budget | Notes |
|------|--------|--------|
| Input → timeline visual update (mouse move while dragging playhead or zooming) | **≤ 1.0 ms CPU** on a mid-range laptop, **≤ 2.0 ms** worst case with 50k clips | No decoder, no alloc storms |
| Full UI frame (IMGUI layout + GPU submit) at 1920×1080 UI | **≤ 4 ms** typical, **≤ 8 ms** worst | Leaves headroom for 120 Hz |
| Timeline draw of N clips in view | **O(visible clips)** not O(total clips) | Viewport culling + banding |
| 10k clips, zoomed out so all visible | **banding**: merge adjacent same-track bars into fewer draw rects | Blick uses banding at high zoom to reduce overdraw |
| 50k clips, zoomed in to ~100 visible | binary search to find visible range | sorted arrays |
| Decode request for monitor | **never on UI thread**; cancel previous in-flight seek | preroll on decode threads |
| First picture after drop | best-effort; UI already showing bars | no “please wait while indexing” |
| Peak RAM (foundation, 1–4 sources) | keep caches bounded (start with 512 MB–2 GB cap, configurable) | Blick cited ~3 GB live caches depending on concurrent sources |

**Interaction-path prohibition list** (compile-time comments + code review checklist):

- No `std::fs` on UI thread except already-mapped memory.
- No FFmpeg / decoder / encoder calls on UI thread.
- No waiting on mutexes held by decode threads (use try_lock or lock-free queues).
- No per-clip `HashMap` lookup in the draw hot loop if an array index will do.
- No `clone()` of large strings/paths per frame.
- No `Vec` realloc in the per-frame timeline draw if it can be a reused scratch buffer.

---

## 2.1 Hardware targets (GPU vs CPU, RAM)

Blick’s published bar (download page): **modern 64-bit CPU, 8 GB RAM or more recommended, GPU with current drivers.** Windows 10/11 x86-64; macOS 15+ **Apple Silicon only**. That is a normal laptop, not a Resolve “32 GB + 12 GB VRAM” box.

### GPU

Blick **needs a GPU to draw** (custom Direct3D 11 on Windows, Metal on Mac). It does **not** need a workstation GPU for the zero-lag timeline.

| Job | Needs a fat GPU? | Reality |
|-----|------------------|---------|
| Timeline zoom/pan/select/30k bars | **No** | Cheap rectangles. Integrated graphics is enough if D3D11/Metal works. |
| UI itself | A **working** GPU | No GPU / broken drivers = no app. Not “must be RTX.” |
| Monitor / scrub picture | **Helps** | Hardware (GPU) or software (CPU) decode. Inspector shows which. Blick has “Force software decoding.” |
| Export | **Helps** | Hardware encode by default; software H.264 fallback. |

Hardware decode (NVDEC / QuickSync / D3D11VA / VideoToolbox) is for **getting frames to the monitor** without melting the CPU. If it’s missing, software decode still works; **bars stay instant**, the **picture** may hitch on heavy 4K/multi-layer.

Foundation target: any GPU that can run `wgpu` (Vulkan/DX12/Metal). Do **not** require a 4090. Log which GPU the app vs FFmpeg each picked (Blick added DXGI adapter logging because this bites people).

### RAM

Wassim on a 30k-clip demo: **~3 GB**, mostly **live caches**, scaling with how many sources are being decoded at once — not with how many bars are on the timeline.

| Machine RAM | Expectation |
|-------------|-------------|
| **8 GB** | Official recommendation. Fine if the OS isn’t already eating 12 GB. |
| **16 GB** | Comfortable everyday path. Bigger live cache, more simultaneous sources. |
| **32 GB+** | Nice for lots of 4K layers. **Not required** for the UI to feel instant. |

The timeline must not balloon RAM with clip count. Frames live in a **bounded** cache (start 512 MB–2 GB, configurable `--cache-mb`). Disk cache (waveforms, Überblick, seek indexes) is a folder, not RAM.

**Agent rule:** 16 GB happy path. Bounded caches. GPU required for drawing, not as a 4K tax on the interaction path.

---

## 3. Design principles (copy into `ARCHITECTURE.md` later)

### 3.1 Data-oriented, not object-oriented

Programs transform data. Clips are POD-like structs in contiguous arrays. Tracks are arrays of clips sorted by start time. IDs are `u32`/`u64` newtypes, not `Rc<RefCell<Clip>>`.

### 3.2 Two worlds

```
┌─────────────────────────────────────────────────────────────┐
│ UI THREAD (sacred)                                          │
│  input → mutate EditorState → imgui → GPU command list      │
│  reads: TimelineSoA (immutable for this frame)              │
│  reads: MonitorFrameSlot (latest ready texture, or none)    │
│  never: decode, encode, disk scan of media bytes            │
└─────────────────────────────────────────────────────────────┘
                              │
          lock-free / mpsc of cheap commands & results
                              │
┌─────────────────────────────────────────────────────────────┐
│ MEDIA / WORKER POOL                                         │
│  decode thread pool, preroll, cancel tokens                 │
│  waveform / color-strip analysis (deferred until on timeline)│
│  autosave, export, media inspect (ffprobe-like)             │
└─────────────────────────────────────────────────────────────┘
```

### 3.3 Immediate-mode UI

Rebuild UI every frame from `EditorState`. Widget identity is positional + stable IDs, not retained DOM.

Do **not** start with a retained GUI toolkit (Iced, Slint, GTK, Qt bindings) for the timeline. Those will fight the 1 ms budget.

Recommended UI strategy for foundation:

- **Phase 0–1:** custom immediate-mode layer on `winit` + `wgpu` (or `egui` *only* for debug/inspector panels, **not** for the timeline canvas).
- Timeline canvas is **immediate GPU rectangles + cached strip textures**, not widgets-per-clip.

If the agent chooses `egui` for the whole app, the timeline **must still** be a single `egui::Painter` / tessellated mesh pass, not one `ui.allocate` per clip.

### 3.4 Cheap clip representation (Blick’s actual trick)

From Blick’s team (paraphrased): the UI is fast regardless of footage because clip representations are **bars**, and bars always render fine.

So:

- On-timeline drawing uses `ClipDraw` records: `track, t0, t1, color, flags`.
- Optional overlay textures: Überblick (color gradient strip), MultiWave (audio bands). These are **precomputed caches**, not live decode.
- Actual pixels live only in the monitor cache.

### 3.5 Invalidation, not polling

- Timeline structural cache rebuilds **only on edit**.
- Media analysis runs **only after a file is placed on the timeline** (not on import of a whole folder).
- Decode requests are **cancelable**; a new scrub supersedes the old one.

### 3.6 Explicit allocators / arenas where it matters

Rust default allocator is fine for the app shell. For per-frame UI and timeline scratch:

- Use a bump arena (`bumpalo`) or a reused `Vec` with `clear()` (capacity retained).
- Avoid `Box` per clip, per widget, per draw command.

### 3.7 Unsafe policy

Allowed, localized, documented:

- SIMD packing of draw vertices
- Mapping GPU buffers
- FFI to FFmpeg
- Lock-free ring buffers if a crate is insufficient

Forbidden as a lifestyle: sprinkling `unsafe` to silence the borrow checker. If ownership is painful, **change the data layout** (SoA, IDs, arenas) rather than cloning everything.

---

## 4. Crate layout (workspace)

Create a Cargo workspace. Keep the binary thin.

```
nle/
  Cargo.toml                 # workspace
  rust-toolchain.toml        # pin nightly only if you must; prefer stable
  crates/
    nle-id/                  # newtype IDs only
    nle-time/                # Time, Frame, Rational, conversion
    nle-media-id/            # MediaId, StreamId
    nle-timeline/            # pure data: Project, Track, Clip, edits, queries
    nle-ui-core/             # immediate-mode widgets, input, layout (no wgpu)
    nle-render/              # wgpu renderer, glyphs, rects, textures
    nle-decode/              # FFmpeg FFI, thread pool, cancel, preroll
    nle-cache/               # frame cache, waveform, überblick strips
    nle-project-io/          # save/load portable project file
    nle-export/              # later; stub ok
    nle-app/                 # the binary: glue
  assets/                    # test media (git-lfs or generate locally)
  docs/
    ARCHITECTURE.md          # copy §3 after Phase 0
    PERF.md                  # how to run benches
  benches/
    timeline_query.rs
    timeline_draw.rs
```

**Dependency direction (strict):**

```
nle-app
  → nle-ui-core, nle-render, nle-decode, nle-cache, nle-project-io, nle-export
nle-decode → nle-time, nle-media-id
nle-cache → nle-time, nle-media-id, nle-decode (or traits only)
nle-timeline → nle-time, nle-id, nle-media-id
nle-render → nle-ui-core (optional), nle-time
nle-ui-core → nle-time, nle-id
```

`nle-timeline` **must not** depend on decode, wgpu, or UI.

---

## 5. Core data model

### 5.1 Time

Use integer time internally. Do not use `f64` seconds as source of truth.

```rust
/// Project-locked unit. 1 tick = 1 / timebase seconds.
/// Recommend timebase = 60000 or lcm of common rates (24/25/30/50/60/120).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct Tick(pub i64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rational {
    pub num: i32,
    pub den: i32,
}

pub struct Timebase {
    pub ticks_per_second: i64, // e.g. 60_000
}

impl Tick {
    pub fn to_seconds(self, tb: Timebase) -> f64 { /* display only */ }
    pub fn from_seconds_round(sec: f64, tb: Timebase) -> Tick { /* input mapping only */ }
}
```

Frame conversion:

```text
frame_index = floor(tick * fps_num / (ticks_per_second * fps_den))
```

Blick allows changing fps/resolution at any time and keeps edits in place. Foundation: store clips in **ticks**, not frames. Project fps is a *display/playback* property, not the storage unit.

### 5.2 IDs

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct ClipId(pub u32);
pub struct TrackId(pub u32);
pub struct MediaId(pub u32);
pub struct StreamId(pub u32);
pub struct EffectId(pub u32);
```

Never store pointers to clips in UI. Store IDs.

### 5.3 Media catalog (not the timeline)

```rust
pub struct MediaItem {
    pub id: MediaId,
    pub path: PathBuf,           // also store optional relative path for portability
    pub kind: MediaKind,         // Video, Audio, Image, Font, Unknown
    pub duration: Option<Tick>,  // none until probed
    pub streams: Vec<StreamInfo>,
    pub probe_state: ProbeState, // Pending, Ready, Failed
}

pub struct StreamInfo {
    pub id: StreamId,
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub fps: Option<Rational>,
    pub sample_rate: Option<u32>,
    pub channels: Option<u16>,
    pub index: i32, // container index
}
```

Probing is a worker job. Timeline can accept a drop before probe completes using a default duration (e.g. 5 seconds) then snap when probe returns — or wait for probe but **still** show a bar.

### 5.4 Timeline (source of truth)

```rust
pub struct Project {
    pub timebase: Timebase,
    pub fps: Rational,          // playback/display
    pub size: UVec2,            // composition size
    pub sample_rate: u32,
    pub tracks: Vec<Track>,
    pub media: MediaLibrary,
    pub playhead: Tick,
    pub in_out: Option<(Tick, Tick)>,
}

pub struct Track {
    pub id: TrackId,
    pub kind: TrackKind,        // Video, Audio
    pub clips: Vec<Clip>,       // ALWAYS sorted by start_tick, non-overlapping per track (foundation)
    pub muted: bool,
    pub locked: bool,
    pub height_px: f32,         // UI concern can live in UiState; keep a copy if you want persistence
}

pub struct Clip {
    pub id: ClipId,
    pub media: MediaId,
    pub stream: Option<StreamId>,
    pub start: Tick,            // on timeline
    pub duration: Tick,         // on timeline
    pub source_in: Tick,        // offset into media
    pub speed: Rational,        // 1/1 for foundation
    pub enabled: bool,
    // NO decoded frames here
    // NO wgpu textures here
    // NO PathBuf here — look up via MediaId
}
```

**Invariant:** `clips` on a track is sorted by `start`. After every edit, restore the invariant (or perform edits as ordered splices).

**Overlap policy (foundation):** forbid overlaps on the same track (razor/overwrite/ripple like a simple NLE). Simpler queries. Lift later if needed.

### 5.5 Derived timeline cache (the Blick “clip states” cache)

Rebuild only when `timeline_generation` increments.

```rust
pub struct TimelineCache {
    pub generation: u64,
    /// Per track, compact SoA for queries + draw
    pub tracks: Vec<TrackCache>,
}

pub struct TrackCache {
    pub track_id: TrackId,
    pub starts: Vec<i64>,     // Tick.0
    pub ends: Vec<i64>,
    pub clip_ids: Vec<ClipId>,
    pub colors: Vec<u32>,     // packed RGBA for bar
    pub media_ids: Vec<MediaId>,
}

impl TrackCache {
    /// First index with end > t, then walk while start < t_end
    pub fn visible_range(&self, t0: i64, t1: i64) -> Range<usize> {
        // binary search on starts/ends
    }
}
```

From Blick (Handmade Network / Discord notes): they store compact begin/end timestamps packed per track, sorted for binary search, invalidated only on changes; at high zoom they band to reduce overdraw.

**Banding algorithm (zoom-out):**

```text
px_per_tick = view.width_px / (view.t1 - view.t0)
If px_per_tick * clip.duration < 0.5 px:
  merge consecutive clips on same track into one rect until merged width >= 1 px
Draw fewer, wider rects. Color = first clip or average.
```

This is how 30,000 clips stay cheap.

### 5.6 View state (UI only)

```rust
pub struct TimelineView {
    pub t0: Tick,          // left edge of view
    pub t1: Tick,          // right edge
    pub y_scroll: f32,
    pub playhead: Tick,    // or take from Project
    pub snap: bool,
    pub tool: Tool,        // Pointer, Razor, Slip, Range
}

pub enum Tool { Pointer, Razor, Slip, RangeSelect }
```

Mapping:

```text
x = (tick - t0) / (t1 - t0) * width
tick = t0 + x / width * (t1 - t0)
```

Use integer math where possible to avoid jitter. For mouse, convert once per event to Tick, then keep ticks.

---

## 6. Immediate-mode UI (foundation)

### 6.1 Frame loop

```text
poll winit events (do not block decode)
for each event: apply to EditorState (cheap)
if redraw needed (always if animating/playing; else on input):
    ui.begin_frame(input, dt)
    layout panels (splits)
    timeline.draw(state, cache, painter)
    monitor.draw(latest_frame_or_black)
    inspector.draw(selected)
    command_palette if open
    ui.end_frame() -> draw list
    renderer.submit(draw list)
    present
```

Target: vsync on, but **input sampling high**. On Windows, use `WaitEventsTimeout` or a 120 Hz tick when playing.

### 6.2 Timeline is not a widget tree

Pseudo-draw:

```text
cull tracks by y
for track in visible_tracks:
    range = cache.visible_range(t0, t1)
    if zoomed_out: band(range) -> rects
    else: one rect per clip in range
    push_rects(rects)
    if zoomed_in enough AND strip_texture exists:
        draw textured quad (überblick / waveform)
draw playhead line
draw selection outline
```

Hit testing: same binary search as visibility. Do not iterate 50k clips.

### 6.3 Tools (foundation set)

Implement these and nothing else first:

| Tool | Behavior |
|------|----------|
| Pointer | click select, drag move (same track), drag edge trim |
| Razor | click splits clip |
| Slip | drag changes `source_in`, duration/start fixed |
| Range | drag a time range, extract/duplicate later |

Hold-to-tool (Blick-like): hold `C` for razor, release to restore pointer. Easy in IMGUI.

### 6.4 Command palette

A string fuzzy-find over a static table of commands. Do this early; it unblocks workflow without menus.

```rust
struct Command {
    id: &'static str,
    name: &'static str,
    key: Option<KeyChord>,
    exec: fn(&mut EditorState),
}
```

### 6.5 Suggested crates for UI shell (not timeline)

| Crate | Use |
|-------|-----|
| `winit` | window/input |
| `wgpu` | GPU |
| `egui` + `egui-wgpu` | **inspector, media panel, menus only** |
| `fontdue` or `glyphon` | text |
| `rfd` | native file dialog |

**Do not** put 10k egui windows on the timeline.

If writing custom IMGUI (preferred long-term), study:

- Casey Muratori — Immediate Mode GUI (talk)
- Ryan Fleury — UI series (dgtlgrove)
- Clay layout algorithm (for nested panels)

Foundation can hybrid: egui chrome + custom timeline canvas.

---

## 7. Rendering

### 7.1 GPU

Use `wgpu`. One device, one queue.

Pipelines:

1. **Solid rect** (timeline bars, playhead, panels)
2. **Textured rect** (monitor, waveforms, überblick)
3. **Glyphs** (timecode, clip labels — only for *visible* clips when zoomed in)

Instancing: upload a vertex/instance buffer of rects once per frame from a reused CPU `Vec<RectInstance>`.

```rust
#[repr(C)]
struct RectInstance {
    pub pos: [f32; 2],
    pub size: [f32; 2],
    pub color: u32,
    pub z: f32,
}
```

Capacity: start with 64k instances. If banding works, 30k clips zoomed out become hundreds of rects.

### 7.2 Monitor

A single texture (or double-buffered pair) written by the decode/composite worker.

Protocol:

```text
UI has: AtomicU64 frame_epoch, Texture
Worker: decode → GPU upload on render thread via queue.write_texture
         or: decode to CPU buffer → UI thread uploads (simpler foundation)
```

Foundation: **CPU decode to RGBA8 buffer → UI thread `queue.write_texture`**. Later: GPU decode / zero-copy.

If the latest frame is stale, still draw it. Never block.

### 7.3 Composite (foundation)

Monitor shows **topmost video clip under playhead** (single stream), optionally with a trivial transform. No blending modes yet.

Query:

```text
for track in video_tracks.rev() {
    if let Some(clip) = track.clip_at(playhead) {
        media_time = clip.source_in + (playhead - clip.start) // ignore speed for v0
        request_decode(media, media_time)
        break
    }
}
```

---

## 8. Decode pipeline (this is what makes scrubbing not suck)

### 8.1 Constraints from video reality

Seeking compressed video is **not random access**. Decoders reconstruct from the previous keyframe (IDR) forward. If you call `seek(currentTime)` on every mouse-move, you will lose.

Therefore:

1. UI thread **never** seeks.
2. Scrub publishes a **desired Tick** (atomic).
3. Decode pool coalesces: only the latest desired time matters.
4. Seek is keyframe-aware + preroll (decode GOP from nearest keyframe).
5. In-flight work is **canceled** when the target jumps far.

Blick 0.1.2 changelog: dedicated decode thread pool, decoder preroll, cancelable requests, instant play without waiting for a full frame-index cache.

### 8.2 Types

```rust
pub struct DecodeRequest {
    pub id: RequestId,          // monotonic
    pub media: MediaId,
    pub stream: StreamId,
    pub when: Tick,             // media timeline
    pub cancel: CancelToken,
}

pub struct DecodedFrame {
    pub request_id: RequestId,
    pub media: MediaId,
    pub when: Tick,
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,    // start with RGBA8
    pub data: Vec<u8>,          // later: pooled buffers
}

pub struct CancelToken {
    inner: Arc<AtomicBool>,
}
```

### 8.3 Threading

```text
UI  --(atomic playhead + mpsc DecodeCmd)--> DecodeScheduler
DecodeScheduler:
  - owns pool of N workers (N = min(4, cores-1) for foundation)
  - one “hot” worker per currently visible source (monitor)
  - on new target: set cancel on old request; enqueue new
  - coalesce: if 20 targets arrive in 16 ms, decode only the last

Workers:
  - keep a sticky decoder per MediaId (don’t reopen the file every seek)
  - if |new_t - last_t| is small and forward: decode forward without seek
  - if jump back or far: seek to previous keyframe, preroll to target
```

**Sticky decoder** is essential. Opening FFmpeg contexts is expensive.

### 8.4 FFmpeg integration — there is no special Blick FFmpeg

Blick does **not** ship a forked/secret FFmpeg. Wassim: “No we’re using FFmpeg.” Decode = anything FFmpeg can decode. They bundle **stock FFmpeg shared libraries** next to the app, donate to the project, and dynamically link for license reasons.

**Use current stable FFmpeg (7.x / 8.x line). Freeze the version. Vendor the shared libs. Do not hunt for a “Blick build.”**

#### License and linking (non-negotiable if the app is proprietary)

Blick (Wassim):

- Dynamically linked “because of GPL, wish we could statically link”
- Encode **H.264 only for now**, “legal constraints, not technical”
- LGPL is what makes FFmpeg viable for a closed app

| Do | Don’t |
|----|--------|
| Link **libavformat, libavcodec, libavutil, libswscale, libswresample** as **DLL/dylib/so** | Shell out to `ffmpeg.exe` on the scrub/monitor path |
| **LGPL** configure if proprietary (no `--enable-gpl`, no libx264 unless you accept GPL) | Static-link a GPL FFmpeg into a closed binary |
| Ship the shared libs beside the exe; codesign dylibs on macOS (Blick had to fix this) | Assume the user already has FFmpeg on PATH |
| Encode: **hardware H.264** and/or **OpenH264**; Blick falls back to software H.264 | Ship x264 in a proprietary NLE without a GPL plan |
| Help → third-party licenses screen | Hide FFmpeg attribution |
| LGPL compliance: allow replacing the FFmpeg libs / provide linkable objects as required | Pretend dynamic link = no obligations |

BtbN / gyan Windows builds are fine for **dev**. For shipping, **build FFmpeg yourself** so you control hwaccel flags, LGPL, and codesigning.

Rust bindings (pick one, pin it): `ffmpeg-next` / `ffmpeg-the-third` / `ffmpeg-sys-next`, or raw C FFI.

#### How to *call* it (this is the special part, not the version)

```text
avformat_open_input
avformat_find_stream_info
find video stream
avcodec_open2          // STICKY — keep AVFormatContext + AVCodecContext per MediaId
seek: av_seek_frame(stream, ts, BACKWARD)  // to previous keyframe
then av_read_frame + send/receive until pts >= target OR cancel
sws_scale to RGBA if needed (or hw download)
```

- UI thread **never** calls into libav*
- Sticky decoder: do not reopen the file every seek
- Cancel token checked in the read/decode loop
- Coalesce: 20 seeks in 16 ms → decode only the last
- Hardware decode when available (D3D11VA / NVDEC / QSV / VideoToolbox), **software fallback**, setting to force software
- Log which GPU FFmpeg picked vs which GPU the renderer picked
- Probe (inspector) is a worker job: every stream’s codec, size, channels, timing

Foundation Phase 3: software decode + cancel/sticky/coalesce is enough to prove the architecture.  
**Shipping:** enable hwaccel. Picture of 4K/multi-layer needs it; bars do not.

### 8.5 Do not require a full frame index before play

Blick 0.1.2: play/scrub immediately without building a complete frame-index cache.

Foundation:

- Optional: build keyframe index **in the background** after first open.
- Until ready, `av_seek_frame` still works (slower, still async).
- Never gate UI on index completion.

### 8.6 Playback vs scrub

| Mode | Decode strategy |
|------|-----------------|
| Play | sequential decode, preroll 10–30 frames, audio sync later |
| Scrub (paused) | coalesced random access, show last decoded, skip intermediates |
| Scrub while playing | Blick shortens audio burst to one project frame; video: same coalesced seeks |

Playback clock:

```text
UI thread or a play thread advances playhead by dt * speed, in ticks.
Do not let decode stall the clock. If frames late, skip (play) or hold last (scrub).
```

### 8.7 Audio (foundation)

- Decode audio on workers too.
- Waveform overview: downsample to peaks (min/max per pixel column) **once**, cache.
- MultiWave (bass/mids/treble) is Phase 2. Mono peak waveform is enough for foundation.
- Output: `cpal` crate. Ring buffer of PCM. If underrun, silence, don’t block UI.

---

## 9. Caches

### 9.1 Kinds

| Cache | Key | Value | When built | Bound |
|-------|-----|-------|------------|-------|
| Probe | MediaId | StreamInfo, duration | on import | tiny |
| Frame (monitor) | (MediaId, approx Tick) | RGBA image | on demand | e.g. 256–1024 frames or MB cap |
| Waveform | MediaId | peak min/max mip pyramid | after clip on timeline | small |
| Überblick | MediaId | 1×N color gradient | after clip on timeline | tiny |
| Thumbnail | MediaId | small JPEG/RGBA | after on timeline | small |
| GOP / keyframe index | MediaId | vec of (tick, file_pos) | background | small |

**Defer analysis until the file is on the timeline** (Blick 0.1.3). Importing 500 files must not launch 500 waveform jobs.

### 9.2 Frame cache policy

- LRU by last use.
- Prefer caching **decoded monitor-sized** frames, not 8K source.
- While scrubbing, don’t cache every intermediate if it would evict useful neighbors; cache a sparse grid (every N ticks) plus the latest.

### 9.3 Memory

Expose:

```text
--cache-mb 2048
```

Default 1024–2048. Never unbounded `Vec` growth.

Use a buffer pool for `DecodedFrame.data` to avoid allocator churn.

---

## 10. Project files

Blick: one portable file in a plain folder; relative + absolute paths; autosave.

Foundation format: **JSON or MessagePack** plus a folder:

```
MyProject.nleproj          # or project.json
media/                     # optional gathered copies (later)
autosave/
```

JSON is fine for foundation (debuggable). Schema version field is mandatory.

```json
{
  "version": 1,
  "timebase": 60000,
  "fps": [30, 1],
  "size": [1920, 1080],
  "tracks": [...],
  "media": [{ "id": 1, "path": "C:/...", "rel": "media/clip.mp4" }]
}
```

Autosave: background thread, atomic write (`write tempfile + rename`). UI never waits.

Opening a project: parse JSON, rebuild `TimelineCache`, kick probe/waveform for used media only.

---

## 11. Editing operations (kernel)

Implement as functions on `Project` that return `EditResult` and bump generation.

```rust
fn razor(project: &mut Project, tick: Tick, track: Option<TrackId>)
fn trim_start(project: &mut Project, clip: ClipId, new_start: Tick)
fn trim_end(project: &mut Project, clip: ClipId, new_end: Tick)
fn move_clip(project: &mut Project, clip: ClipId, new_start: Tick, new_track: TrackId)
fn slip(project: &mut Project, clip: ClipId, delta_source: Tick)
fn delete(project: &mut Project, clip: ClipId)
fn overwrite_drop(project: &mut Project, media: MediaId, at: Tick, track: TrackId)
```

**Undo:** command stack of inverse edits (not full project snapshots). Foundation: store inverse ops.

```rust
enum Edit {
    InsertClip { clip: Clip, track: TrackId },
    RemoveClip { clip: Clip, track: TrackId, index: usize },
    PatchClip { id: ClipId, before: Clip, after: Clip },
}
```

Cap undo at 256 entries.

---

## 12. Rust-specific guidance (so the agent doesn’t drown)

### 12.1 Ownership layout that works

Bad:

```rust
struct Clip { frames: Vec<Frame>, decoder: Box<dyn Decoder> }
struct App { clips: Vec<Rc<RefCell<Clip>>> }
```

Good:

```rust
struct App {
    project: Project,              // owned, UI mutates via edit fns
    cache: TimelineCache,          // derived
    ui: UiState,
    media_tx: Sender<MediaCmd>,
    latest_frame: Arc<Mutex<Option<ReadyFrame>>>, // or crossbeam + epoch
}
```

Workers never borrow `Project`. They receive **copies of the tiny info they need** (`MediaId`, path, tick).

### 12.2 Sharing paths and strings

Store paths once in `MediaLibrary`. Clips hold `MediaId`. Clone of `PathBuf` per clip is how RAM dies at 30k clips.

### 12.3 Parallelism

- `rayon` is OK for **offline** analysis (waveforms).
- Do **not** rayon the UI frame.
- Decode pool: dedicated threads, not rayon tasks, because sticky decoders + cancel.

### 12.4 Interior mutability

Prefer:

- UI thread owns `Project`
- Atomics for playhead mirroring to workers
- `crossbeam::channel` for commands/results
- `parking_lot::Mutex` only for the latest-frame slot, held for microseconds

If you need `RwLock<Project>` shared with workers, you already failed §0.

### 12.5 Generics and traits

Keep traits small and at boundaries:

```rust
trait FrameSink { fn submit(&self, frame: DecodedFrame); }
trait MediaDecoder { fn seek_and_decode(&mut self, t: Tick, cancel: &CancelToken) -> Result<DecodedFrame>; }
```

Do not abstract “ClipWidget”. Do not introduce an ECS. Do not introduce async (`tokio`) in the UI process for foundation — it fights the frame loop. `std::thread` + channels is enough.

(Async can wrap FFmpeg later; it is not the foundation.)

### 12.6 Error handling

- Decode failures: log + black frame, editor stays up.
- Missing media file: offline clip color (e.g. magenta bar), still editable.
- Never `unwrap` on media I/O in the hot path.

### 12.7 Logging / tracing

`tracing` crate. Span the decode worker, not every rect. A frame-time HUD (ms for ui, draw, wait) is mandatory in debug builds.

---

## 13. Suggested crate versions / stack (agent should pin actual latest)

| Need | Crate |
|------|--------|
| Window | `winit` |
| GPU | `wgpu` |
| Chrome UI | `egui`, `egui-wgpu`, `egui-winit` |
| Channels | `crossbeam-channel` |
| Atomics extras | `arc-swap` (optional, for latest frame) |
| FFmpeg | `ffmpeg-next` or current maintained binding |
| Audio out | `cpal` |
| Dialog | `rfd` |
| Serialize | `serde`, `serde_json` |
| IDs | local newtypes |
| Math | `glam` |
| Image | `image` (png thumbnails) |
| Fuzzy command palette | `fuzzy-matcher` |
| Profiling | `tracing`, `puffin` or `tracy-client` (debug) |

Avoid: `ffmpeg` CLI via `std::process` for the monitor path (ok as a last-resort export stub only). Avoid ffmpeg.wasm. Avoid assuming a special Blick FFmpeg exists.

---

## 14. Phased execution plan

Each phase has **exit criteria**. Do not start the next phase until they pass.

### Phase 0 — Empty window, IMGUI chrome, 60+ FPS empty timeline (3–7 days)

**Build:**

- Workspace + crates skeleton
- `winit` + `wgpu` + egui chrome
- Split panels: media (empty), monitor (black), timeline (ruler + tracks empty)
- Playhead drag on empty timeline
- Zoom (wheel), pan (mmb / alt-drag)
- HUD: frame time, clip count (0), view range
- `tracing` + debug vsync toggle

**Exit:**

- Window opens in < 1 s
- Empty timeline zoom/pan at ≥ 60 FPS, p95 frame < 8 ms
- No FFmpeg linked yet (optional)

### Phase 1 — Timeline kernel + 50k fake clips (3–7 days)

**Build:**

- `Project` / `Clip` / `Track` / `TimelineCache`
- Generator: N dummy clips
- Viewport cull + binary search + banding
- Select / move / trim / razor on dummy clips (no media)
- Undo/redo
- Command palette: add track, razor, delete

**Exit:**

- 50,000 dummy clips, zoomed out: ≥ 60 FPS, interaction path < 2 ms
- 50,000 dummy clips, zoomed in: same
- Unit tests: sort invariant, `clip_at`, razor splits duration, undo restores

This phase **proves Blick’s bar trick** before FFmpeg complexity exists.

### Phase 2 — Media import + probe + bars from real files (3–7 days)

**Build:**

- Drop files / file dialog
- Worker: FFmpeg probe (`StreamInfo`, duration)
- `overwrite_drop` onto timeline
- Clip color from media id hash
- Inspector shows codec/resolution (Blick “know exactly what is in every file”)
- Analysis jobs **queued only when clip is placed**

**Exit:**

- Drop a 4K file: bar appears immediately (duration may update when probe returns)
- Importing 200 files does not freeze UI
- Offline/missing file = magenta bar, no crash

### Phase 3 — Decode pool + monitor scrub (the hard phase) (1–2 weeks)

**Build:**

- Sticky FFmpeg decoder per media
- Cancel tokens + coalesced target tick
- Preroll from keyframe
- Monitor texture update
- Background GOP index (optional)
- Software decode RGBA

**Exit:**

- Scrubbing a long GOP H.264 file: timeline never hitching
- Monitor may blur/stale during fast scrub; catches up
- Play at 1x for a short clip without audio is acceptable
- Killing the app during decode is clean (Join handles on drop)

**Tests:**

- Cancel: enqueue 100 seeks, only last completes
- Worker does not call into UI
- Memory cap: cache does not grow without bound during a 60 s scrub

### Phase 4 — Audio + waveforms + play (1 week)

**Build:**

- Audio decode + `cpal` output
- Peak waveform mip cache
- J/K/L or space play/pause
- Playhead chase
- Mute track

**Exit:**

- Play A/V roughly in sync (±1 frame later to tighten)
- Waveforms draw from cache, not live FFT
- Scrub while paused shows still frame, optional analog-style audio blip (short)

### Phase 5 — Project IO + autosave (2–4 days)

**Build:**

- Save/load JSON
- Relative path resolution
- Autosave thread
- Open last project (optional)

**Exit:**

- Kill -9 analog: restart recovers last autosave
- Move project folder with relative media still opens

### Phase 6 — Export stub → real encode (1 week)

**Build:**

- Export job on worker
- UI remains editable during export (Blick: editing doesn’t stop for a render)
- Cancel export
- Foundation encode: one video stream, no effects, FFmpeg encode H.264 + AAC

**Exit:**

- Start export, keep trimming clips, export either uses a snapshot of the timeline at start **or** live-updates with documented semantics. **Pick snapshot-at-start for foundation** (simpler, still non-blocking).

### Phase 7 — Polish that still belongs to “foundation”

- Hold-to-tool keys
- Linked clips (A/V) move together
- Timecode display
- Snap to clip edges
- Media panel list (search later)
- Linux window/package follow-up only after the Windows editor is polished (optional)

Stop. This is the foundation. Effects, speed, multiple timelines, Linux packaging, macOS, and
plugins are post-foundation.

---

## 15. Tests the agent must write in Phase 1 (before any UI glamour)

File: `crates/nle-timeline/src/lib.rs` (and `tests/`)

1. `sorted_after_insert`  
2. `clip_at_playhead`  
3. `visible_range_matches_brute_force` for random clips  
4. `razor_preserves_media_and_source_in`  
5. `trim_does_not_invert_duration`  
6. `undo_stack_roundtrip`  
7. `banding_reduces_draw_count_when_zoomed_out`  
8. `50k_visible_range_is_sub_millisecond` (bench or ignored-by-default heavy test)

File: `crates/nle-decode/tests/` (Phase 3)

1. `latest_seek_wins`  
2. `cancel_stops_preroll`  
3. `sticky_decoder_reused`  

Use a tiny generated MP4 (few frames) checked into `assets/` or created via FFmpeg in the test setup.

---

## 16. Agent working rules

1. **Phase 0–1 first.** If you open FFmpeg in Phase 0, you are procrastinating on the architecture.
2. Every UI-thread function that is called from the frame loop gets a comment: `// HOT PATH — no IO`.
3. Clip count in HUD. Frame time in HUD. Always.
4. Do not add effects “while you’re there.”
5. Do not introduce Tokio, Bevy, or an ECS.
6. Do not use `Rc<RefCell<>>` for clips.
7. Prefer `u32` IDs + `Vec` over maps in hot structures. Maps are OK for `MediaId → MediaItem` (hundreds, not millions).
8. When the borrow checker hurts, **split structs** (Project vs UiState vs Caches) rather than cloning the timeline.
9. Measure before optimizing decode. The first optimization is cancel + coalesce + sticky decoder, not SIMD IDCT.
10. Keep the binary startable: `cargo run -p nle-app`.
11. FFmpeg is stock, shared, LGPL. Do not invent a fork. Do not call it from the UI thread.
12. Target 16 GB RAM machines. Bound the frame cache. Require a GPU for drawing, not a gaming GPU for bars.

---

## 17. First commands for the local agent

```bash
cargo new nle --bin --name nle-app
# then convert to workspace as in §4

# after Phase 1:
cargo test -p nle-timeline
cargo bench -p nle-timeline   # once benches exist

# never:
cargo add bevy
```

Minimum `nle-app` main (conceptual):

```rust
fn main() {
    // init tracing
    // create channels
    // spawn decode scheduler (even if stub)
    // run winit event loop
}
```

---

## 18. Mapping: Blick feature → foundation analog

| Blick | Foundation |
|-------|------------|
| Written from scratch in Odin | Written from scratch in Rust; still from scratch (custom timeline + decode) |
| Custom renderer | `wgpu` immediate rects |
| Custom IMGUI | Hybrid: egui chrome + custom timeline |
| Bars independent of footage | `TimelineCache` + banding |
| Compact clip state cache, binary search | `TrackCache` SoA |
| Live caches ~GBs | bounded `nle-cache` |
| Decode thread pool, preroll, cancel | `nle-decode` |
| Analysis deferred until on timeline | job queue gated by clip placement |
| Change fps anytime | ticks as source of truth |
| Edit during export | snapshot export on worker |
| Autosave | background atomic write |
| Command palette | static command table |
| Hold-to-tool | key-down tool override |
| Überblick / MultiWave | Phase 4+ cached strips |
| FFmpeg inspector | probe worker + inspector panel |
| Bundled FFmpeg shared libs (LGPL) | vendor LGPL FFmpeg DLLs; no GPL x264 unless license allows |
| Hardware or software decode | hwaccel + force-software setting; inspector shows which |
| 8 GB RAM recommended; ~3 GB live caches | bounded `nle-cache`; 16 GB happy path |
| D3D11 / Metal custom renderer | `wgpu` (DX12/Vulkan/Metal) |

---

## 19. What “success” looks like for the foundation

You should be able to say:

> I dropped a long H.264 file on a timeline that already had 20,000 dummy (or real) bars, zoomed from 5 seconds to 5 hours and back, dragged the playhead as fast as my hand moved, and the **timeline never felt like it waited**. The monitor was allowed to be a frame or two behind. CPU in the interaction path stayed under 2 ms.

If the monitor is perfect but the timeline stutters, the project failed.  
If the timeline is perfect and the monitor is occasionally stale, the project succeeded.

---

## 20. Optional stretch (only after Phase 5)

- Überblick color strip (sample 1px columns offline)
- MultiWave (3-band audio texture)
- AV/linked clips
- Hardware decode
- Rate stretch tool
- Live keyframing of a single transform (position/scale) on the clip — still no node graph

---

## 21. Document history / intent

This plan exists because Blick’s speed is **architectural**. A Rust NLE that uses a retained UI toolkit and decodes on scrub events will feel like every other editor. A Rust NLE that treats the timeline as bars, caches derived clip state, and runs a cancelable sticky-decoder pool can feel like Blick.

There is **no special FFmpeg**. There is **no 4090 requirement** for the feel. There is a sacred UI thread, LGPL shared FFmpeg, bounded caches, and 50k bars before decode.

Start with 50k bars. Earn the right to decode.

| Ver | Date | Change |
|-----|------|--------|
| 1.0 | 2026-08-23 | Initial foundation plan |
| 1.1 | 2026-08-23 | Two-machines model; GPU/RAM targets; FFmpeg is stock LGPL shared libs; must-employ checklist |

---

## 22. Appendix A — `visible_range` reference

```rust
impl TrackCache {
    pub fn visible_range(&self, t0: i64, t1: i64) -> std::ops::Range<usize> {
        // first clip with end > t0
        let start = self.ends.partition_point(|&e| e <= t0);
        // first clip with start >= t1
        let end = self.starts.partition_point(|&s| s < t1);
        start..end.max(start)
    }
}
```

Brute-force test against this for random data. This function is the heartbeat of the 50k-clip budget.

---

## 23. Appendix B — Coalescing decoder scheduler (reference)

```text
loop {
    recv timeout 4ms:
        drain all pending DecodeCmd
        keep only the latest target per (media, stream)
        if target changed: cancel current, start worker job
    handle completed frames:
        if request_id == latest_id: publish to latest_frame slot
        else: drop (stale)
}
```

Stale frame drop is a feature. It is how fast scrubs stay cheap.

---

## 24. Appendix C — Review checklist for every PR

- [ ] UI thread functions in the PR: any IO? any FFmpeg?
- [ ] New per-clip heap alloc in the frame loop?
- [ ] Timeline still O(visible) / O(log n + visible)?
- [ ] Cache bounded?
- [ ] Cancel path tested if decode involved?
- [ ] HUD still shows frame time?
- [ ] Phase exit criteria still hold?
- [ ] FFmpeg still shared/LGPL, not CLI-on-scrub, not GPL-static?
- [ ] UI thread still does not call libav*?

If any box is unchecked, do not merge.

---

## 25. Must-employ checklist (Blick stack, minus Odin)

FFmpeg is the **media spine**. It is not the product. If the agent skips this list, the editor will not feel like Blick.

### Architecture (non-negotiable)

1. Timeline = bars / SoA cache / binary search / banding — **zero FFmpeg on that path**
2. Decode thread pool, sticky decoder, **cancel + coalesce** latest playhead
3. Live frame cache, **bounded** (GBs, not unbounded)
4. Analysis (waveforms, color strips, frame index) **only after a clip is on the timeline**
5. Instant first play — do **not** wait for a full frame-index

### Graphics

6. Native GPU renderer (`wgpu` ≈ Blick’s D3D11/Metal). A working GPU is required to draw; a 4090 is not.
7. Timeline = instanced rects, not one widget per clip
8. Monitor = one (or double-buffered) texture; stale frame OK

### UI

9. Immediate-mode. Chrome can be egui; **timeline canvas must not be**
10. Hold-to-tool, command palette, no modal “new project” lock

### Audio

11. Separate from video decode (`cpal` or similar)
12. Cached peak waveforms; optional pitch-preserving stretch later (Blick: spectral / speech / varispeed)

### App / legal / ship

13. Probe via FFmpeg (inspector = every stream’s codec, size, timing, last decoder hardware vs software)
14. Third-party licenses screen
15. Tiny native binary + **bundled** FFmpeg shared libs — not Electron, not ffmpeg.wasm
16. Codesign FFmpeg dylibs on Mac
17. If proprietary: **LGPL FFmpeg, dynamic link**, H.264 encode via hardware and/or OpenH264 — not GPL x264 unless you accept GPL

### Do not employ

- ffmpeg.wasm / CLI for playback
- Tokio, Bevy, or an ECS for the editor
- `Rc<RefCell<Clip>>` with frames inside the clip
- A hunt for “special Blick FFmpeg” — it does not exist
- A requirement that users own a discrete GPU for the timeline to be fast

### Practical Rust stack

- `ffmpeg-next` / `ffmpeg-sys-next` (or raw FFI) against **your** shared FFmpeg
- `winit` + `wgpu`
- `crossbeam` + dedicated decode threads
- `cpal`
- `rfd` for dialogs

**If the agent does one FFmpeg-specific thing first:** sticky decoder + cancelable keyframe seek, dynamically linked LGPL libs, hwaccel with software fallback. Everything else Blick-like is the timeline, not a custom FFmpeg.

---

**End of plan.** The local agent should create the workspace, complete Phase 0, then Phase 1 with the 50k-clip benchmark before touching FFmpeg.
