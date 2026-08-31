# Phase 1 isolated latency comparison gate

`scripts/Run-Phase1LatencyComparison.ps1` is an opt-in, cargo-only local
measurement. It does not launch the Maelstrom GUI or any application EXE.

```powershell
.\scripts\Run-Phase1LatencyComparison.ps1
```

It reuses `Run-Phase1Multisource.ps1` to create and verify the same four
dynamic 1920x1080/30fps, five-second MPEG-4, 30-frame-GOP fixtures. The exact
ignored release test then performs 20 one-source and 20 four-source trials,
interleaved with the first scenario alternating by trial parity. Every raw
sample has a strict `sequence_index` from 0 through 39. Each trial creates and
drops a fresh `App`, selects Full preview quality, explicitly requests
1920x1080 output, and uses one of five deterministic mid-GOP ticks. This
prevents sticky sessions and frame caches from crossing between scenarios.

The timer begins before `set_playhead` and immutable preview request creation.
Each raw sample records input-to-submit time and time until matching source
frames are ready, requested/decoded ticks (no more than one 30fps frame, or
33,334 microseconds, after the requested tick), media IDs, Full output
dimensions, observed decoder backend, pool diagnostics, and post-drop session release.
Schema version 1 writes atomically to the ignored local report
`artifacts/phase1-latency/phase1-latency-comparison.json`. It includes raw
samples, nearest-rank p50/p95/max summaries, and safe deltas/ratios.

The gate enforces only the headless scheduler path: the four-source
input-to-submit p95 must be at most 1 ms. It does not prove visible UI latency.
Frame-ready timing is intentionally report-only: it includes local Software
decode performance, has a coarse five-millisecond polling granularity, and has
no established cross-machine baseline. The report is atomically written with a
`passed` or `failed` status before the threshold assertion so failed raw samples
survive. The runner validates report shape, exact sample counts and sequence,
summary math, finite comparison values, output/tick/backend and release evidence
before printing `PASS` or reporting the preserved report path.
The validator accepts the finite JSON numeric forms produced by both Windows PowerShell 5.1
(`System.Decimal`) and modern PowerShell (`System.Double`); this changes report portability, not
the 1 ms absolute scheduler threshold.

This is local Software-backend evidence, not sustained playback, a visible UI
responsiveness measurement, GPU-compositor proof, or cross-hardware completion.
The broader Phase 1 exit gate remains unchecked.

After the app-wide decoded-frame cache consolidation, the gate passed again on
2026-08-29 with a four-source input-to-submit p95 of 71 us and coarse
frame-ready p95 of 93 ms. The ignored report is
`artifacts/phase1-latency/phase1-latency-shared-cache.json`.

After source-owned actor/session deduplication, the same gate passed on
2026-08-29 with a four-source input-to-submit p95 of 144 us and coarse
frame-ready p95 of 78 ms. The current ignored report remains
`artifacts/phase1-latency/phase1-latency-comparison.json`.

After exact rational source-rate propagation and unknown-timing exact-cache semantics, the gate
passed on 2026-08-30 with a four-source input-to-submit p95 of 140 us and coarse frame-ready p95 of
82 ms. The same run recorded a one-source scheduler p95 of 59 us and five peak source-owned
sessions under the hard cap of eight.

## Cached-reply observation repair (2026-08-31)

The paused/full-resolution workload exposed an evidence race: a prewarm worker could
publish pixels into the shared cache before its producer event reached the app. A later
cache reply could replace that event in the latest-wins slot. Cached replies correctly
carry no backend, so the app's event-only history could remain empty even after successful
decoding. The baseline gate failed its nonempty-backend assertion; this was not a measured
performance failure and its log remains preserved.

Each decoder lane now keeps a six-bit, monotonic record of backends that successfully
packed a frame. Recording happens before traversal or final frames become cache-visible.
The app polls this bounded evidence independently of frame delivery. It includes successful
unpresented work and is lifetime evidence, not proof of the current frame's producer, a
particular source's backend, hardware transfer, or presentation. Cache-hit backend/fallback
fields and per-media/active-preview labels retain their existing semantics. Failed opens
and cache-only consumers cannot create observations. Timing/report schema remains version 1.

The test remains paused, with full 1920x1080 output and normal prewarming. Its wait now
requires every requested layer to have a frame **and** a completed request, rather than
accepting a progressive frame alone. Neither quality nor the 1 ms scheduler limit changed.

Three consecutive repetitions passed, each with 20 one-source and 20 four-source trials:

| Run | One-source submit p95 (us) | Four-source submit p95 (us) | One-source frame-ready p95 (ms) | Four-source frame-ready p95 (ms) |
|---|---:|---:|---:|---:|
| 1 | 82 | 270 | 45 | 70 |
| 2 | 74 | 189 | 42 | 70 |
| 3 | 121 | 246 | 42 | 69 |

All 120 trials observed Software, retained Full output and matching source identity/ticks,
stayed within the eight-session cap, and released every session after App drop. These are
fresh-App headless measurements with five-millisecond polling, not sustained playback,
physical input/scanout latency, or a speedup over the assertion-failing baseline.

Evidence lives under ignored `artifacts/phase1-multisource/`:

- `backend-observation-before-latency.log`: original gate's missing-backend assertion.
- `backend-observation-regression-before-mpeg4.log`: new deterministic integration test
  fails with an empty observation list against the old app polling behavior.
- `backend-observation-regression-after.log`: the same test passes with the fix. It consumes
  an actual decode event and a subsequent cache event without presenting either, then proves
  app polling recovers successful-work history without inventing a presented frame.
  This models a missed event; record-before-cache ordering is additionally code-reviewed.
- `backend-observation-regression-before.log`: unsuccessful PNG fixture experiment, retained
  separately; this runtime could not open the PNG decoder, so it is not regression proof.
- `backend-observation-latency-{1,2,3}.json` and matching logs: raw performance evidence.
  The unchanged runner's read-only validation statements were applied to all three reports,
  including source paths/sizes, forty ordered samples, distributions, ratios, and limits;
  session-cap bounds were also checked without regenerating historical fixtures/reports.
- `backend-observation-workspace.log`: 747 release tests passed, 21 opt-in tests ignored.
- `backend-observation-hardware.log`: four explicit D3D11VA/DXVA2 H.264/HEVC tests passed
  152 exact timing/pixel comparisons, including native 1080p. Default-adapter evidence only.
- `backend-observation-clippy.log`: strict release workspace/all-target lint gate passed.
  Formatting and independent read-only review also passed.

SHA-256 of the three latency reports, in run order:

```text
853AF94CC5C999C23FB739616436DE9C8F38A82D54662ACDC6A929EAC942DE08
75FBB06D1886E15866FA59A5FB47F382BFBFB0F0E2AAD5BA9DA4FF29B8675E3E
225A93E80DEAF69002A5F3D6EE9C3117DB5F1D0BF9F5633874D97E498531F53A
```

The portable package was rebuilt without launching the editor. Its executable matches
the release build, SHA-256 `0F30B9388BB8861B8622CA2768CD6B0F36585FBBA829D5206C68189A82C40534`.
`PACKAGE-STATUS.json` explicitly retains `smoke_status: not_run`. Both runtime-only batch
checks, packaged FFmpeg/FFprobe loader checks on a minimal PATH, and copied runtime hashes
passed. The previous executable/status are recoverable in
`backend-observation-previous-package.zip`; existing historical smoke reports remain untouched.
No editor was launched and no task-owned compiler/test/media process remained after checks.
Local evidence manifest: `backend-observation-verification.json`, SHA-256
`60A041CA4206FE10D2516CA18F6F5015C086E3B611DE046FC03C608DC73A142A`.
