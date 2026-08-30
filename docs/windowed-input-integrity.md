# Windowed scrub input integrity

The windowed probe now preserves a failed pending sample and records bounded input
diagnostics. A deterministic test reproduces incoming pointer motion changing the
probe's held ruler drag. The origin of the two earlier interrupted runs remains
unproven: their reports did not capture backend input. No input is discarded and
no workload, pixel identity, resource, or CPU acceptance rule is relaxed.

## Observed mechanism and reporting repair

The app collects an `egui-winit` input batch before the probe adds its synthetic
events. While waiting for matching layers, the probe adds no further movement,
but its synthetic pointer remains pressed. An incoming `PointerMoved` therefore
reaches the ordinary ruler drag handler and changes the playhead. This is expected
editor input behavior, but it invalidates the fixed benchmark workload.

`phase1_ui_backend_motion_during_wait_preserves_failed_sample_evidence` exercises
that path through actual egui layout and the production ruler handler. It verifies
that the existing guard fails when the target changes. An otherwise eligible paint
is then supplied on the failure frame. Previously, `presented` could consume that
pending sample despite the recorded failure, erasing its diagnostic context and
adding it to the completed samples. It now returns immediately after a failure,
including either timeout. The report retains the original pending target and the
first failure. Failed runs remain failures, regardless of their partial timings.

Restoring only the old presentation guard makes the regression fail at
`assertion failed: probe.samples.is_empty()`; restoring the correction passes.
The logs are `windowed-input-preservation-{before,after}.log` under
`artifacts/phase1-multisource/`.

## Bounded diagnostics

During the controlled held gesture, the probe retains only the latest nonempty
backend input batch summary, captured before its own injection. A failed report
includes event category counts, focus, the final pointer position, elapsed time,
pending sample index, and the playhead/scrub state before and after egui processing.
It also records whether that frame injected a measured synthetic movement.

The summary never retains text, key values, clipboard contents, device identifiers,
or screenshots. There is no event history or growing buffer. A test supplies 10,000
clipboard events and verifies that serialization remains below 512 bytes and omits
their private payload. Passing reports contain no backend input summary.

This is input-batch evidence, not physical-device attribution. The latest batch
may precede the actual failure, and its timestamp and before/after state must be
considered before drawing conclusions. The probe does not isolate or suppress user
input. Future unattended runs still require an undisturbed editor window; any
interruption must remain disclosed rather than be retried until a pass appears.

## Verification — 2026-08-30

One authorized four-case run completed without a workload interruption:
`windowed-1e11e7cd-3285-4104-8884-4c1f4a1b6f9d`. It used the approved launcher,
Full-1080p requests and surface, Software decoding, and the unchanged eight warmups
plus forty measured inputs per case.

| Adapter | Sources | Input CPU p95 (ms) | Frame CPU p95 (ms) | Matching-frame p95 (ms) |
|---|---:|---:|---:|---:|
| Intel UHD 770 | 1 | 0.4226 | 1.0963 | 61.8216 |
| Intel UHD 770 | 4 | 0.5295 | 1.2883 | 134.6029 |
| RTX 3090 | 1 | 0.4021 | 0.8576 | 67.5441 |
| RTX 3090 | 4 | 0.4329 | 1.0950 | 85.8907 |

All cases pass input CPU p95 ≤1 ms, frame CPU p95 <8 ms, and exact native layer
identity checks. Cache peaks remain below 1 GiB; session peaks are two for one
source and seven for four sources, below eight. This run does not establish the
cause of the historical interruptions or prove that they cannot recur. The
134.6029 ms Intel result is retained; no new timing threshold or performance
improvement is claimed for this diagnostic change.

The 36,140,544-byte package matches the release build, SHA-256
`BCCA5054FC2966948F3DD0D510693B3AC6D17A759A920834CCA24418B00C2B1C`.
General package smoke remains `not_run`. The preceding seek-fix package is
preserved as `verified-seek-Maelstrom.exe` with its package status in the earlier
`windowed-70128ce5-d307-46db-892b-e25179687593` directory. The two historical failed
wrapper reports remain unchanged, as does their exclusion from passing evidence.

Release workspace verification passes 737 tests, with 16 opt-in tests ignored;
the existing supplied H.264 and reordered VFR fixtures are enabled. Strict
all-target release Clippy, formatting, the valid-report control and 25 corruption
rejections, Windows process ownership checks, and independent review pass.

`Verify-WindowedInput.ps1` rechecks all four cases through the production validator,
configuration/report/fixture/executable hashes, unchanged historical failed reports,
the previous package archive, regression logs, and the absence of editor/compiler/
media-tool processes. Its `windowed-input-verification.json` output has SHA-256
`943AA27AF444EE1EB713206481371D4DA941B64479149FFF77B15726899E6E49`.

These are local synthetic-input measurements on two adapters in one machine.
They do not establish physical input/scanout latency, sustained playback/audio
continuity, broader codec/backend behavior, or the reference-machine exit gates.
Continue those separate roadmap tasks while retaining evidence for any future
input-integrity interruption.
