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

This is local Software-backend evidence, not sustained playback, a visible UI
responsiveness measurement, GPU-compositor proof, or cross-hardware completion.
The broader Phase 1 exit gate remains unchecked.

After the app-wide decoded-frame cache consolidation, the gate passed again on
2026-08-29 with a four-source input-to-submit p95 of 71 us and coarse
frame-ready p95 of 93 ms. The ignored report is
`artifacts/phase1-latency/phase1-latency-shared-cache.json`.
