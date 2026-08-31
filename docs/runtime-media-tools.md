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

The portable package was rebuilt from source commit `5e391770aa5e568e047fe17055f8b4944687e224`.
Its executable SHA-256 is
`C42458E86AE972F2C985083A3B5686BA48F6CF5B2F098DDFC3F9059A5C23C72F`. The complete previous
23-file package is retained in a verified ZIP with SHA-256
`579FD305C30C29275523B0DDF575896F4C67DACC0D880E1F784D2C2F16D618AB`. Only
`Maelstrom.exe` and `PACKAGE-STATUS.json` changed. All 13 pinned runtime copies match, all 15 PE
files are AMD64 with only adjacent, present Windows-system, or identified API-set imports, both
media-tool loader checks pass on a restricted path, and the exact full-path launcher passes
`--verify-runtime`. The retained evidence is
`artifacts/phase1-multisource/package-media-tools-5e39177/verification.json`. GUI smoke was not run.
