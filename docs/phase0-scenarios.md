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

The report has `schema_version: 2`, an explicit scenario count, and six scenarios. Each scenario records its name, finite
iteration count, elapsed milliseconds, observable decoder backend when one was
produced, and explicit pass/failure evidence. The matrix covers public monitor
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

The runner restores altered environment variables. The test removes only its
own fixture copy and export final/staging/filter files; the successful JSON
report remains for inspection. Generated fixtures and reports are local-only
artifacts and are intentionally not part of the public repository.
