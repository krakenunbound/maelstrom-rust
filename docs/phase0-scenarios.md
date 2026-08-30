# Phase 0 scenario harness

`scripts/Run-Phase0Scenarios.ps1` runs the finite, ignored native Rust scenario
matrix. It does not launch the Maelstrom GUI executable.

The runner requires an explicit absolute FFmpeg 8.1 bundle path. It generates
and validates the tracked fixture contract, then passes only the generated
two-second MP4 to the test through `MAELSTROM_PHASE0_MEDIA`. The JSON report is
written atomically to the ignored `artifacts/phase0-scenarios/` directory.

```powershell
.\scripts\Run-Phase0Scenarios.ps1 -FfmpegRoot 'H:\Maelstrom Rust\.deps\ffmpeg-project-8.1'
```

The report has `schema_version: 4`, an explicit scenario count, and seven scenarios. Each scenario records its name, finite
iteration count, elapsed milliseconds, observable decoder backend when one was
produced, observable encoder backend when one was produced, and explicit
pass/failure evidence. Decoder and encoder evidence use separate nullable fields
so an encoder can never be reported as a decoder. The matrix covers public monitor
decoder reverse scrubs, editor-state switching, offline-media recovery,
runtime video-strip eviction, and cancellation of an actual FFmpeg export with
no output left behind. The cache checkpoint allocates five deterministic 70 MiB
RGBA strips (350 MiB cumulative, 280 MiB live before eviction), checks the 256
MiB cap after every insertion, and requires exact oldest-first retention of
strips 3–5 (210 MiB retained). Its evidence records `cumulative_bytes`,
`retained_bytes`, `cap_bytes`, and `peak_live_bytes`. The peak is modeled live
RGBA payload, not an operating-system RSS/commit measurement. Allow roughly
280 MiB plus test-process overhead; this opt-in matrix is intentionally serial.

The `four_source_decoded_frame_cache_pressure` scenario runs one bounded pass using four distinct
fixture paths; its `iterations: 4` field counts the four exercised sources. It sizes the decoded-frame cache for exactly three 160x90 RGBA frames (57,600 bytes
each), forces real decoded-frame LRU eviction, and proves four exact foreground source groups/actors,
current/peak byte bounds, session/source/actor caps, and zero post-release resources. It does not prove idle/session LRU
eviction and does not replace the 600-second playback soak or cross-hardware gates.

The `multi_source_pressure_and_idle_retirement` scenario uses twelve distinct copies of the
same validated fixture in three batches of four concurrent production `MonitorDecoder` instances.
All batches share one three-frame 160x90 RGBA decoded-frame cache, one foreground-only four-session
pool, and one four-source coordinator. Every batch requires four decoded frames and concrete decoder
backends, exactly four foreground sessions/source groups/lane actors within their hard caps, then
explicitly releases every decoder and waits for sessions, groups, live actors, and retiring actors to
reach zero before the next batch. Its `iterations: 12` evidence records source/batch/lane counts,
cache current/peak/cap/evictions, peak ownership bounds, all three idle-release cycles, and final zero
ownership. Twelve distinct cache keys require at least nine real LRU evictions under the three-frame cap.

The runner restores altered environment variables. The test removes only its
own fixture copy and export final/staging/filter files; the successful JSON
report remains for inspection. Generated fixtures and reports are local-only
artifacts and are intentionally not part of the public repository.

## Sustained scenario matrix

`scripts/Run-Phase0SustainedScenarios.ps1` repeatedly runs the same validated
seven-scenario matrix for a requested wall duration. Its default 600-second run is
the authoritative local duration; 15–599 second runs are explicitly
non-authoritative harness checks.

```powershell
& 'H:\Maelstrom Rust\scripts\Run-Phase0SustainedScenarios.ps1' `
    -FfmpegRoot 'H:\Maelstrom Rust\.deps\ffmpeg-project-8.1' `
    -DurationSeconds 600
```

The orchestrator never launches the GUI or a raw generated executable. It calls
the existing Cargo-backed scenario runner, performs the fixture generation and
manifest gate on the first pass, then reuses that validated fixture for later
passes. Both scripts anchor Cargo to the repository, so the full-path command is
independent of the caller's current directory. The schema-version 1 report is written atomically under
`artifacts/phase0-sustained-scenarios/` and contains requested duration, separate
setup and matrix wall durations, authoritative status, machine and pinned-FFmpeg
evidence, every child run and its seven scenario records, aggregate per-scenario
run/work-iteration and elapsed-time totals, and any available failed-child
scenario evidence. A user-local cross-process mutex serializes the shared fixture
and cancellation artifacts.

This gate supplies repeated reverse-scrub, project-switch, offline/recovery,
three forms of cache/idle-retirement pressure, and export-cancellation evidence in one report. It
does not display the UI, exercise a live audio device, measure physical GPU
scanout, replace the packaged playback soak, or satisfy the integrated/discrete
cross-hardware exit gate by itself.

The retained authoritative local schema-4 checkpoint on 2026-08-30 ran from source
commit `266309f595baf6aedbe15c30dcc1c99d05fe281e`. It passed for 600.436 seconds,
excluding 1.348 seconds of setup, with 520 complete matrix runs, 3,640 scenario
executions, and 19,240 declared work iterations. Every scenario passed all 520
runs; all child reports had unique SHA-256 values and no invocation/report-read
failure. The Software decoder and `h264_mf` encoder appeared only in their
separate nullable role fields. The report recorded `authoritative: true` with
SHA-256 `ca531cbe16f5ccfc2d8085efdc30cbb674bba911d9505cfba2f39054b6d8b05f`.
The schema-1 wrapper does not yet embed a Git revision; the source commit above was captured from
the unchanged tracked `HEAD` immediately before starting the tracked runner session.
Integrated/discrete cross-hardware proof remains open.
