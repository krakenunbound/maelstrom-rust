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
foreground sessions, three top-layer speculative background sessions, a peak
of seven, and a cap of eight. It also proves that all monitor requests
completed, records applicable decoder backend identities, and that dropping
the app releases every session.

The atomically written report is local-only at
`artifacts/phase1-multisource/phase1-multisource.json`. Schema version 1
records the absolute fixture paths and sizes, decoded IDs and source ticks,
requested source tick, Full-quality output size, submission and all-frame
timing, applicable decoder backends, complete lane/session metrics, and
post-drop active sessions. The runner validates every required value before
printing `PASS`.

This is a preliminary bounded-session/full-output local scheduler gate. Its
single submission measurement is a local threshold, not a timeline-latency
regression baseline, p95, sustained playback, GPU compositing benchmark, or
cross-hardware claim. A comparable one-source baseline and sustained/p95
timeline gate remain open because adding them here would not use the identical
full scheduler path without broadening this focused proof. Those Phase 1 exit
items remain required.
