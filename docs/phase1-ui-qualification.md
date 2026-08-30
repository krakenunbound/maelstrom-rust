# Phase 1 windowed four-source qualification

The four-source UI/cross-hardware exit gate remains open. This document records the
preparation and missing measurement work; it is not a passing performance report.

## Existing evidence and limits

| Existing path | What it proves | Missing for this gate |
|---|---|---|
| `Run-Phase1LatencyComparison.ps1` | Isolated one/four-source Full-1080p scheduler and matching-frame timings | No window, input dispatch, or compositor |
| `Run-Phase1SustainedSoak.ps1` | Ten-minute headless four-source cache/session bounds | No window or timeline responsiveness measurement |
| `Run-Phase0CrossAdapterSurface.ps1` | Per-adapter packaged surface, GPU, decoder, and audio activity | One 320x180 source; no per-source four-layer proof or timeline latency comparison |

`App::start_media_acceptance_smoke` imports one path from
`MAELSTROM_MEDIA_ACCEPTANCE_PATH`; `advance_media_acceptance_drag_smoke` exercises
one layout-backed media drop. `SurfaceSubmissionProbe` samples 120 CPU frames,
surface submission intervals, and `frame.present()` call durations. None of these
measurements is physical input-to-display latency. See
[`performance-reports.md`](performance-reports.md) for the GPU and display limits.

## Remaining implementation

- Add an opt-in app probe that reuses production import, timeline, preview, and
  native viewer paths with four distinct verified 1920x1080 sources. Record exact
  media, request, generation, source-time, output-resolution, and backend evidence
  for each contributing layer; aggregate upload counts alone are insufficient.
- Compare equivalent one-source and four-source timeline interactions, with fixed
  workload, warmup, sample count, and raw samples. Keep handler/draw CPU timing,
  request-to-matching-frame timing, GPU completion, and surface submission timing
  separate. Define the measurement boundaries before applying the unchanged
  roadmap limits; do not relabel scheduler or present-call timing as input-to-visual.
- Reuse strict integrated/discrete adapter selection and preserve driver, display,
  cache/session, codec, and executable/fixture hash provenance. An RTX 3090 result
  would be local discrete-adapter evidence, not a mid-range reference-machine result.
- Add a bounded runner using the exact approved launcher, with attributable app
  process ownership, timeout/failure reports, and cleanup of owned descendants.
  Existing direct-executable runners must not be invoked by agents under AGENTS.md.
- Validate report and failure paths without a GUI first. Only then request explicit
  permission to open the editor for the prepared live workload. Adapter presence
  alone does not prove successful selection, decoding, or responsiveness.

## Package preparation

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
