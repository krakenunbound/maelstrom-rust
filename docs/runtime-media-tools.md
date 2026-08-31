# Runtime media-tool ownership

Maelstrom resolves its shared FFmpeg runtime once with the existing startup-resources worker. The
interface thread stores the result and clones the already-resolved FFmpeg path into Quick Export,
Kraken Upscale, and proxy requests; those actions no longer inspect the filesystem or search the
process `PATH` to discover a tool.

## Resolution contract

The worker checks these locations in order:

1. `ffmpeg` and `ffprobe` beside the running Maelstrom executable (the portable-package layout).
2. `FFMPEG_DIR/bin` (the approved developer/test bundle).

Both tools must be ordinary files in the same directory. Their paths are canonicalized and must be
absolute. Maelstrom never combines one executable from each location and never falls back to an
unqualified command name. A missing or partial runtime does not prevent the editor from opening;
dependent actions remain unavailable and report the stored reason in English or Japanese.

The background export, upscale, and proxy owners retain their own execution-time validation and
failure reporting. Startup resolution is a stable routing decision, not a claim that a file cannot
be removed or replaced later.

## Verification

Two focused regressions cover adjacent-package priority, whole-pair fallback, canonical absolute
paths, and refusal to use ambient `PATH`. The existing asynchronous proxy failure regression injects
a missing absolute executable and proves the interface action starts the worker without rechecking
that path. At this stopping point, all 806 release workspace tests pass (24 ignored), as do the two
explicit real-media proxy tests, strict Clippy, formatting, all seven fixture contracts, and all
seven Phase 0 scenarios. No editor window was launched; packaged GUI qualification remains open.
