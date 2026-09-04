# Phase 0 scenario harness

`scripts/Run-Phase0Scenarios.ps1` runs the finite, ignored native Rust scenario
matrix. It does not launch the Maelstrom GUI executable.

The runner requires an explicit absolute FFmpeg 8.1 bundle path. It generates and validates the
tracked fixture contract, validates/rebuilds the separately pinned local-only AOM AV1 fixture, then
passes exact absolute fixture paths through environment variables. Before the seven-scenario matrix,
focused Cargo tests gate waveform timestamp-origin normalization, software or qualified named decode
against independent CLI pixels, app preview floor/hold addressing, and source-identity export across
the reordered TS, shifted/reordered MPEG-4, shifted 10-bit MOV, and shifted AV1 fixtures. AV1 uses
`MAELSTROM_AV1_VFR_TEST_MEDIA`; the runner restores it with every other altered variable. The JSON
report is written atomically to the ignored `artifacts/phase0-scenarios/` directory.

```powershell
.\scripts\Run-Phase0Scenarios.ps1 -FfmpegRoot 'H:\Maelstrom Rust\.deps\ffmpeg-project-8.1'
```

The report has `schema_version: 4`, an explicit scenario count, and seven scenarios. Each scenario records its name, finite
iteration count, elapsed milliseconds, observable decoder backend when one was
produced, observable encoder backend when one was produced, and explicit
pass/failure evidence. Decoder and encoder evidence use separate nullable fields
so an encoder can never be reported as a decoder. The matrix covers public monitor
decoder reverse scrubs, headless Software delayed-event editor-state switching,
offline-media recovery,
runtime video-strip eviction, and cancellation of an actual FFmpeg export with
no output left behind. The cache checkpoint allocates five deterministic 70 MiB
RGBA strips (350 MiB cumulative, 280 MiB live before eviction), checks the 256
MiB cap after every insertion, and requires exact oldest-first retention of
strips 3–5 (210 MiB retained). Its evidence records `cumulative_bytes`,
`retained_bytes`, `cap_bytes`, and `peak_live_bytes`. The peak is modeled live
RGBA payload, not an operating-system RSS/commit measurement. Allow roughly
280 MiB plus test-process overhead; this opt-in matrix is intentionally serial.

`rapid_editor_state_switching` initializes two project snapshots, then performs exactly eight alternating
snapshot restores. It retains one completed real 160x90 Software event, then holds a distinct real
request in flight at a test-only worker boundary and switches projects before consuming it. After
release, normal production cancellation must suppress that in-flight request; zero session,
source-group, live-actor, and retiring-actor ownership is required before continuing. The retained
real event names the prior monitor generation and is
rejected without changing pixels, offline state, or error state. A fresh request is then consumed
and must present with the new project's media/path/playhead identity. The report requires exactly
eight cancellation suppressions, eight stale prior-generation rejections, and eight fresh post-switch
presentations, followed by zero session, source-group, live-actor, and retiring-actor ownership.
This is headless Software decoder/session-lifecycle evidence: it does not claim GUI/audio/hardware/
scanout behavior, playback quality, or broader cross-hardware qualification.

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

## Packaged disruption schedule

The live packaged disruption schedule is deliberately separate from this headless Rust matrix.
`scripts/Run-PlaybackDisruptions.ps1` creates an owned 60-second 1920x1080 A/V fixture only for an
opt-in run, starts the editor only through the exact project batch launcher with `--cache-mb=512`,
and validates the schema-1 app report for eight scrubs, eight restoring frames and frame-gated
playback/audio-transport restarts, offline/error and
recovery behavior, real decoded-frame-cache eviction, and cancelled-export cleanup. Its
`-ValidateOnly` mode proves launcher/package/runtime identity without GUI, FFmpeg, environment, or
artifact side effects. The harness exists but has not yet produced live, scanout, or cross-hardware
Phase 0 evidence; it withholds a passed wrapper until owned-process, environment, and disposable
artifact cleanup verifies, including a settled export with no residue. See `docs/performance-reports.md`
for the full contract.

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
independent of the caller's current directory. The schema-version 2 report is written atomically under
`artifacts/phase0-sustained-scenarios/` and contains requested duration, separate
setup and matrix wall durations, authoritative status, machine and pinned-FFmpeg
evidence, every child run and its seven scenario records, aggregate per-scenario
run/work-iteration and elapsed-time totals, and any available failed-child
scenario evidence. A user-local cross-process mutex serializes the shared fixture
and cancellation artifacts.

Schema 2 embeds `source_revision` snapshots from before and after the matrix: the absolute Git
executable, exact commits, tracked/submodule dirtiness while ignoring untracked-only content,
commit stability, qualification, and a bounded nullable capture error. A 600-second request fails
before the matrix if its start state is unavailable or tracked-dirty, and fails at publication if
the commit or tracked state changes. Short dirty-tree harness checks may pass but remain explicitly
non-authoritative.

This gate supplies repeated reverse-scrub, project-switch, offline/recovery,
three forms of cache/idle-retirement pressure, and export-cancellation evidence in one report.
The multi-source scenario now also fills a two-source coordinator, requires a third real source to
defer, applies the production priority-first least-recently-requested selection to a completed
lower-priority group, and proves the exact retained request retries without displacing the newer
resident. This is demand-driven capacity reclamation, not a background or time-based idle reaper. It
does not display the UI, exercise a live audio device, measure physical GPU
scanout, replace the packaged playback soak, or satisfy the integrated/discrete
cross-hardware exit gate by itself.

The retained authoritative local schema-2 wrapper over the schema-4 matrix on 2026-08-30 embeds
identical clean start/end source commit `99d43d65d87474f71b83361cec5d5f79a69e4532`.
It passed for 600.673 seconds, excluding 1.133 seconds of setup, with 521 complete matrix runs,
3,647 scenario executions, and 19,277 declared work iterations. Every scenario passed all 521
runs; all child reports had unique SHA-256 values and no invocation/report-read
failure. The Software decoder and `h264_mf` encoder appeared only in their
separate nullable role fields. The report recorded `authoritative: true` with
SHA-256 `bef925939b118aaf7d9c1339cbd6e0cfca1c084b0e7b57a46d24971f0ba1e5d6`.
Integrated/discrete cross-hardware proof remains open.

The 2026-09-01 requalification embeds identical clean start/end source commit
`b668543b15d0eb2e2bb53d1540fe9dae206dbd2b`. It passed for 617.912 measured matrix seconds,
excluding 1.210 seconds of setup, with 32 complete matrix runs, 224 scenario executions, and
1,312 declared work iterations. All seven scenarios passed all 32 runs, every child report had a
unique SHA-256, and no invocation or report-read failure occurred. The wrapper is authoritative;
its SHA-256 is `80C965A68011CC0CEB950744176C803857E70CBF104054B99D15B07A3D871FE5`.
Every multi-source run recorded one idle reclaim, exact retry identity, the newer resident still
owned, and zero final session/source/actor ownership.
It observed only the headless Software decoder and `h264_mf` encoder in their explicit role fields;
the listed Intel/NVIDIA adapters were machine inventory, not exercised rendering. Live audio,
GUI-present, packaged playback, physical scanout, and cross-hardware soak evidence remain open.

The 2026-09-04 requalification embeds identical clean start/end source commit
`2b89378d53181689bd76930e67994de61fbc7f02`. It passed for 602.640 measured matrix seconds,
excluding 1.377 seconds of setup, with 41 complete matrix runs, 287 scenario executions, and
1,681 declared work iterations. All seven scenarios passed all 41 runs, all 41 child-report hashes
were unique, and no invocation or report-read failure occurred. Every rapid-switch run recorded
exactly eight in-flight cancellation suppressions, eight stale prior-generation rejections, eight
fresh post-switch presentations, eight monitor-generation advances, eight media-analysis epoch
advances, zero monitor errors, and zero final session/source/actor ownership. The authoritative
schema-2 report SHA-256 is
`285E2523B8CF07F37256EFC13481DA5B5A4C31C708ECF246A03DFF57B701B895`.
It renews headless Software decoder and `h264_mf` encoder evidence only; GUI, live audio,
packaged playback, renderer GPU, physical scanout, and cross-hardware qualification remain open.
