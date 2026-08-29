# Performance reports

Maelstrom's existing surface-submission probe writes one versioned JSON report when
`MAELSTROM_SURFACE_SUBMISSION_REPORT` names an output file. It measures a fixed 120-frame window;
report serialization and disk IO remain on the existing one-shot worker.

The report records:

- CPU frame-time p95, surface-submission interval p95, and average submission rate;
- the renderer adapter, vendor/device IDs, wgpu backend, and driver strings reported by wgpu;
- every decoder backend that produced an observed monitor frame and the encoder process most
  recently started by the export fallback chain;
- CPU identity, logical processor count, and total physical RAM;
- selected and resolved preview quality, requested decode dimensions, monitor-cache cap, and
  current display refresh rate.

This is CPU and submission-cadence evidence. It does not claim scanout, GPU-completion, or end-to-end
display latency. A standalone cadence probe may report an empty decoder list and
`encoder_backend: "not_observed"` when that run did not exercise media. The full package media smoke
defers the report until a decoder has produced a frame and an encoder process has actually started,
so its backend fields are evidence rather than hardware guesses. Project files never contain this
session-only metadata.

Facts unavailable through a supported platform API are serialized as JSON `null`; zero and
`"Unknown"` are not used as hidden unavailable-value sentinels.

Windows packaging validates that the full report includes real CPU/RAM, renderer, media backend,
preview, cache, and display data when available before accepting the build. The retained report is
`dist/last-surface-submission-smoke.json`.
