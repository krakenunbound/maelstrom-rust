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

The report has `schema_version: 1`. Each scenario records its name, finite
iteration count, elapsed milliseconds, observable decoder backend when one was
produced, and explicit pass/failure evidence. The matrix covers public monitor
decoder reverse scrubs, editor-state switching, offline-media recovery,
runtime video-strip eviction, and cancellation of an actual FFmpeg export with
no output left behind.

The runner restores altered environment variables. The test removes only its
own fixture copy and export final/staging/filter files; the successful JSON
report remains for inspection. Generated fixtures and reports are local-only
artifacts and are intentionally not part of the public repository.
