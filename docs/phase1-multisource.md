# Phase 1 four-source decode gate

`scripts/Run-Phase1Multisource.ps1` is the opt-in native gate for the first
multi-source playback scheduler checkpoint. It does not launch the Maelstrom
GUI executable and does not modify a project document.

```powershell
.\scripts\Run-Phase1Multisource.ps1
```

The runner requires the repository's pinned local FFmpeg 8.1 bundle at
`H:\Maelstrom Rust\.deps\ffmpeg-project-8.1` and the local libclang binding
runtime. It creates four separate dynamic five-second, video-only MPEG-4
sources at 1920x1080 and 30 fps from `testsrc2` with distinct hue filters and
30-frame GOPs, then sets absolute media and report environment paths
only for the focused release test. All changed environment variables are
restored in `finally`.

The ignored test builds exactly four video tracks at the same playhead,
submits an immutable paused Full-quality request with an explicit 1920x1080
output size at a 1,500,000 microsecond mid-GOP source tick, and requires the
submission call to return in less than 20 ms. It waits no longer than five
seconds for four independent decoded media IDs and source ticks at or after
the requested tick, then proves the shared pool's exact paused state: four
foreground sessions, one source-owned speculative background session, a peak
of five, and a cap of eight. The three monitor layers requesting the same
speculative source share that one physical background actor/session. It also proves that all monitor requests
completed, records applicable decoder backend identities, and that dropping
the app releases every session.

The atomically written report is local-only at
`artifacts/phase1-multisource/phase1-multisource.json`. Schema version 1
records the absolute fixture paths and sizes, decoded IDs and source ticks,
requested source tick, Full-quality output size, submission and all-frame
timing, applicable decoder backends, complete lane/session metrics, live source
groups, live/retiring source-owned lane actors, and
post-drop active sessions. The runner validates every required value before
printing `PASS`.

The post-source-actor local checkpoint passed on 2026-08-29 with 147 us
submission, all four Full-1080p frames ready in 82 ms, four source groups, five
live actors/sessions (four foreground plus one shared speculative background),
zero retiring actors at the sample, and zero sessions after app drop.

## Priority and eviction semantics

Source capacity is managed globally by the app's four monitor slots. A request
that cannot acquire a new physical source group first releases every speculative
background lane. If group pressure remains, it yields one complete eligible
visual source group using this deterministic order:

1. strictly lower visual priority;
2. oldest latest-request ID;
3. visually lowest layer.

Every logical layer sharing the selected media/path/backend identity is selected
as one group, then its logical leases are yielded sequentially without blocking
on actor shutdown. Any equal- or higher-priority contributor protects that
identity. The decoder retains each yielded lane's exact latest request while
actor shutdown and join run through the bounded reaper; the app keeps the last
completed frame visible and retries in the same priority/topmost order. Explicit
release does not create retry work. Reverse-scrub lanes remain visible work and
are not classified as speculative prewarm. The policy is intentionally strict:
a lower-priority source can remain deferred while a higher-priority source keeps
capacity; no age-based fairness bound is claimed.

Audio playback is owned independently and is not evicted or truncated by monitor
pressure. Current regression coverage proves that the editor's complete audio
playback-target snapshot is unchanged across visual takeover; it is not yet a
live audio-device continuity or underrun proof.

Deterministic decoder and app tests prove speculative-prewarm-first release,
preservation of a visible reverse-scrub lane, active real-media lower-to-top
takeover at a one-group cap, permit-safe retry while the retiring actor still
holds its session, shared-source protection, oldest-first selection,
live-plus-retiring actor bounds, final zero sessions/actors, and the unchanged
editor audio-target snapshot. These tests establish the scheduling invariants;
the UI-present, live-audio, and cross-hardware Phase 1 exit proof remains open.

This is a preliminary bounded-session/full-output local scheduler gate. Its
single submission measurement is a local threshold, not a timeline-latency
regression baseline, p95, sustained playback, GPU compositing benchmark, or
cross-hardware claim. A comparable one-source baseline and sustained/p95
timeline gate remain open because adding them here would not use the identical
full scheduler path without broadening this focused proof. Those Phase 1 exit
items remain required.
