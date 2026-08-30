# Phase 1 windowed four-source qualification

Latest follow-up: [scrub seek performance](scrub-seek-performance.md) records the
subsequent seek/EOF fixes, two clean final-package repetitions (four-source p95
74–104 ms), and two workload-integrity invalidations. The measurements below
remain historical evidence for the earlier timestamp-rounding fix.

The local one/four-source windowed CPU gate passes on Intel UHD 770 and NVIDIA
RTX 3090 after correcting a timestamp-rounding completion bug. The broader
four-source UI/cross-hardware exit gate remains open: these are two adapters on
one machine, and fresh four-source images still take 431–474 ms at p95 in this
software-decoding scrub workload. This is not sustained playback qualification.

## Existing evidence and limits

| Existing path | What it proves | Missing for this gate |
|---|---|---|
| `Run-Phase1LatencyComparison.ps1` | Isolated one/four-source Full-1080p scheduler and matching-frame timings | No window, input dispatch, or compositor |
| `Run-Phase1SustainedSoak.ps1` | Ten-minute headless four-source cache/session bounds | No window or timeline responsiveness measurement |
| `Run-Phase0CrossAdapterSurface.ps1` | Per-adapter packaged surface, GPU, decoder, and audio activity | One 320x180 source; no per-source four-layer proof or timeline latency comparison |
| `Run-Phase1Windowed.ps1` | Passing local windowed CPU/input measurements with exact layer presentation evidence | Reference machines, broader media/backends, and sustained playback remain pending |

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
Source timestamps share the decoder's existing one-microsecond early allowance
for FFmpeg rescaling versus upward-rounded rational frame boundaries; two
microseconds early is rejected. The existing 33,334-microsecond forward bound and
all identity checks remain unchanged.

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

## Authorized live results — 2026-08-30

All four cases passed through the approved launcher with 48 validated inputs each
(eight warmups and forty measured samples), Full 1920×1080 per-source requests,
and a 1920×1080 DX12 surface. All observed decoder backends were `Software`.

| Adapter | Sources | Input CPU p95 (ms) | Full frame CPU p95 (ms) | Matching layers to surface p50 / p95 / max (ms) |
|---|---:|---:|---:|---:|
| Intel UHD 770 | 1 | 0.3178 | 1.0598 | 6.9265 / 334.3945 / 570.6799 |
| Intel UHD 770 | 4 | 0.3596 | 1.3925 | 170.7140 / 473.8643 / 685.9765 |
| NVIDIA RTX 3090 | 1 | 0.3407 | 0.8291 | 6.7937 / 315.8973 / 540.1669 |
| NVIDIA RTX 3090 | 4 | 0.3456 | 0.9353 | 152.6506 / 431.3164 / 431.5187 |

Both CPU limits remain unchanged: input p95 ≤1 ms and frame CPU p95 <8 ms.
Fresh-frame waiting is measured separately and has no invented passing threshold.
The four-source p95 increase over one source is 139.4698 ms on Intel and
115.4191 ms on NVIDIA; responsiveness work remains despite the CPU pass.
Cache peaks were 1,069,977,600 bytes below the 1 GiB cap; peak sessions were two
for one source and seven for four sources, below eight. No monitor errors occurred.
Drivers were Intel `32.0.101.6129` and NVIDIA `32.0.16.1047`.

The authoritative local run is
`artifacts/phase1-multisource/windowed-40b45f22-6f88-4afd-a9bf-b6af537ef072/`.
Its `windowed-wrapper.json` SHA-256 is
`DAECDBBEC314FE7242BE8C6D354FA9F65D5F62EE123C6D61E6A0340B16A5576B`.
The 36,156,416-byte packaged executable SHA-256 is
`3C1DE471175E011D2A68A46E4D1CD6BCDC262619BF5696D32BE6750DF8377183`.
Package smoke status remains `not_run`: this dedicated workload does not replace
the package's general smoke suite. All four editor instances and owned children
exited; no editor, Cargo, compiler, FFmpeg, or FFprobe process remained at the live
run's cleanup check.

### Failure and repair

The initial live attempt (`windowed-8d6eb4f9-0622-4fb2-b573-9f40afd44352`) timed
out on sample 1. A diagnostic rerun
(`windowed-cd71d8c5-ae70-4b6e-a8cb-a04f9f4109c9`) retained the pending sample:
request 7 asked for 1,433,334 µs, while the same native frame was decoded and
painted at 1,433,333 µs. The decoder already accepted this rounding difference,
but app completion and probe matching did not, leaving the request in flight.
Both now use the decoder's existing predicate. The original reports and both
pre-fix executables are preserved inside their respective run directories.
Failure reports also retain one bounded pending sample, four observed/accepted
layer slots, presentation serials, and current scheduler state; these diagnostics
cannot count an incomplete sample as completed.

## Verification and remaining qualification

The release workspace passes 730 tests (16 opt-in tests remain ignored) and strict
all-target Clippy. New headless
tests exercise the real ruler gesture, stale/missing layer rejection, clip/request
identity, timeout reports, writer teardown, and geometry persistence boundaries.
The app regression proves the rounded current frame clears in-flight/deferred
state while stale requests/generations and two-microsecond preroll cannot complete.
The PowerShell validator accepts exact and rounded controls and rejects 25 corrupted reports;
a short hidden PowerShell helper verifies actual Windows CIM dates, mismatched-PID
protection, and owned-process cleanup. No editor is launched by those checks.
Logs are `windowed-rounding-{focused,real-decode-test,workspace-tests,clippy,report-tests}.log`
under `artifacts/phase1-multisource/`. Independent source review approved the fix.
`windowed-rounding-verification.json` independently revalidates all four reports,
configuration/fixture/report hashes, release/package identity, and cleanup. Its
SHA-256 is `F9967E406BDBA58A0B5CD26DF8078923B540B8198B935429E00652667510F3AB`.

Broader media/backend and reference-profile evidence is still required. This RTX
3090 result is local discrete-adapter evidence, not a mid-range reference-machine
result. The paused scrub workload does not qualify sustained playback, audio
continuity, physical input latency, GPU completion per sample, or display scanout.
The user explicitly authorized these launches; future editor launches still follow
AGENTS.md and the approved launcher.

### Earlier harness preparation

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
Independent source review approved that preparation; it predates the live results above.

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
