# Phase 1 windowed four-source qualification

The four-source UI/cross-hardware exit gate remains open. The windowed harness is
implemented and headlessly verified, but has not been run with the editor open.
This document is not a passing windowed performance report.

## Existing evidence and limits

| Existing path | What it proves | Missing for this gate |
|---|---|---|
| `Run-Phase1LatencyComparison.ps1` | Isolated one/four-source Full-1080p scheduler and matching-frame timings | No window, input dispatch, or compositor |
| `Run-Phase1SustainedSoak.ps1` | Ten-minute headless four-source cache/session bounds | No window or timeline responsiveness measurement |
| `Run-Phase0CrossAdapterSurface.ps1` | Per-adapter packaged surface, GPU, decoder, and audio activity | One 320x180 source; no per-source four-layer proof or timeline latency comparison |
| `Run-Phase1Windowed.ps1` | Prepared windowed workload and strict report validator | Actual editor runs and reference-machine qualification remain pending |

`App::start_media_acceptance_smoke` imports one path from
`MAELSTROM_MEDIA_ACCEPTANCE_PATH`; `advance_media_acceptance_drag_smoke` exercises
one layout-backed media drop. `SurfaceSubmissionProbe` samples 120 CPU frames,
surface submission intervals, and `frame.present()` call durations. None of these
measurements is physical input-to-display latency. See
[`performance-reports.md`](performance-reports.md) for the GPU and display limits.

## Implemented workload

```powershell
# Prepare four cases and verify runtime/fixtures; does not open the editor.
.\scripts\Run-Phase1Windowed.ps1
# Headless report-corruption and process-ownership checks; does not open the editor.
.\scripts\Test-Phase1WindowedReport.ps1
# Only after explicit permission to launch the editor:
.\scripts\Run-Phase1Windowed.ps1 -Run
```

Each invocation creates a fresh ignored `artifacts/phase1-multisource/windowed-<UUID>/`
directory containing four schema-1 configurations and a wrapper report. The run
mode opens a fresh process for each one-source/four-source case on each requested
integrated/discrete adapter. The existing four distinct MPEG-4 fixtures are probed
for 1920x1080, 30 fps, and at least five seconds; their hashes and stream metadata,
and the packaged executable/configuration hashes, are retained.

`phase1_ui.rs` registers imports through production APIs, queues normal background
analysis, creates five-second video clips, and lays four sources into separate
quadrants. Catalog loading/saving is disabled and the temporary editor is
save-blocked. Full preview requests explicitly use 1920x1080 per source, independent
of panel size; this is a deliberate stress override, not normal Full quality's
viewer-pixel sizing. The window requests a 1920x1080 physical surface.

After analysis and startup readiness, the probe presses the actual ruler handle
using retained widget geometry and injects 48 pointer moves through egui: eight
warmups, then forty measured forward/backward targets. It checks the exact expected
playhead, freezes timeline structure, and releases the gesture at the end. It does
not call `set_playhead` for measured interactions. Configuration, missing layers,
wrong identities, five-second sample deadlines, and a 150-second total deadline
fail closed. A bounded worker publishes the report and is joined at teardown.

Every measured layer correlates its consuming clip, media, request, generation,
source tick, Full output raster, and native upload serial. The renderer retains
separate upload, composed, and canvas-blit serials; a sample completes only when
all exact accepted uploads appear in a subsequent canvas blit. Cached frames keep
an unobserved backend as JSON `null`; the run must still contain an actually
observed decoder backend. This does not assign a decoder identity to a cache hit.

Raw sample timings are separate:

- `input_to_ui_cpu_ms`: injected ruler motion through completion of the egui UI
  pass, before native callback submission/tessellation. Normal p95 limit: 1 ms.
- `full_cpu_frame_ms`: the complete render CPU interval for that input frame,
  ending before `frame.present()`. P95 must remain below 8 ms.
- `input_to_surface_submission_ms`: the first input frame's present-call return.
- `matching_layers_to_surface_ms`: the later present-call return after all exact
  source uploads reach the native canvas blit. Decode waiting remains report-only.

The validator recomputes nearest-rank distributions from forty measured samples,
checks exact identity/count/order, raster/adapter requirements and resource bounds,
and preserves failing reports. One-versus-four deltas are reported without inventing
a relative threshold. GPU pass/completion summaries remain separate observations;
none of these intervals measures physical input, DWM, or display scanout. Full CPU
timings cover the input frames, not every intervening decoder event or render frame.

The runner invokes only `H:\Maelstrom Rust\Launch-Maelstrom-Editor.bat`.
`MAELSTROM_LAUNCHER_WAIT=1` retains that launcher until its child exits; ordinary
launch behavior is unchanged. PID, exact path, and UTC creation time bind ownership.
Failures clean up owned descendants, and environment values are restored. Existing
direct-executable runners remain unsuitable for agents under AGENTS.md.

## Verification and remaining qualification

The release workspace passes 728 tests and strict all-target Clippy. New headless
tests exercise the real ruler gesture, stale/missing layer rejection, clip/request
identity, timeout reports, writer teardown, and geometry persistence boundaries.
The PowerShell validator accepts a valid control and rejects 23 corrupted reports;
a short hidden PowerShell helper verifies actual Windows CIM dates, mismatched-PID
protection, and owned-process cleanup. No editor is launched by those checks.

Actual one/four-source windowed results are still required, as is broader media and
reference-profile evidence. An RTX 3090 result would be local discrete-adapter
evidence, not a mid-range reference-machine result. This paused scrub workload also
does not qualify sustained playback or audio continuity. Opening the editor for the
prepared runs requires an explicit user request under AGENTS.md.

The refreshed 36,104,192-byte package has executable SHA-256
`E988D988B37DD8E5B31F338AD9A97E34628C5B00A920911D405C95C4776664F8`,
matches the release build, and retains `smoke_status: not_run`. Its previous
package is preserved in `artifacts/phase1-multisource/windowed-package-backup-20260830/`.
The final prepared cases are in
`artifacts/phase1-multisource/windowed-8b3b4db9-135b-4151-a80e-10fedd85760c/`;
all four configuration hashes were independently checked and no app reports exist.
`artifacts/phase1-multisource/windowed-readiness-verification.json` records test,
package, and preparation provenance, SHA-256
`0D7949D669138252CFD581E771AAFE04B61D8E337D3EF66AB4289B1CC493FE57`.
Independent source review approved the final path. Live behavior remains unverified.

## Earlier package preparation

`scripts/package-windows.ps1 -SkipSmoke` supports preparing the current executable
without opening the editor. It retains runtime/model validation and writes
`PACKAGE-STATUS.json` with the executable SHA-256 and `smoke_status: not_run`.
Historical `dist/last-*-smoke.json` reports are preserved, not adopted as proof for
the new executable. The build-only path does not qualify startup, playback, audio,
GPU rendering, or the four-source gate.

The previous package and smoke reports are archived locally under the ignored
`artifacts/phase1-multisource/package-preparation-20260830/` directory. No editor
launch is needed for packaging, hash checks, or the approved launcher's
`--verify-runtime` file-presence check.

On 2026-08-30, the build-only path completed against source HEAD `98a4b72` with only
packaging/documentation edits. The 35,930,624-byte packaged executable matched the
release build exactly, SHA-256
`243E82DE308F1B58A38251E42944AA142164D31BFBD5FD04B9EBC03DF89D88CF`.
The launcher file check passed, as did packaged FFmpeg and FFprobe loader checks
with PATH restricted to Windows directories. Thirteen FFmpeg bundle artifact
copies matched the approved source bytes, all three historical smoke reports
remained byte-identical, and ten environment values were restored after packaging.
No editor was launched and no task-owned process remained.

The local `verification.json` in the preparation directory has SHA-256
`DC607C4CECF3C1DE0171A76ED9F7328E2F17C97D9A1FEA63ADE40694F81BA974`;
`build-only.log` and the two packaged tool version logs retain supporting evidence.
The GUI smoke remains `not_run`; neither this package nor the live four-source
workload is qualified by these checks.
