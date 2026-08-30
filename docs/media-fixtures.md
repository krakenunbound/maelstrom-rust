# Media fixtures

`fixtures/media/manifest.json` is the versioned contract for deterministic media used by the
local harness. The media binaries themselves are generated below `artifacts/media-fixtures`, which
is ignored by Git. Each manifest entry records its ID, recipe, provenance, SHA-256, byte size,
container/stream contract, duration, and whether probing must succeed or fail.
Schema version 2 adds explicit video timing metadata. The VFR fixture records its frame count,
first and last presentation timestamps in microseconds, required distinct timestamp gaps, and a
SHA-256 fingerprint of the comma-separated microsecond PTS sequence.

Use only the pinned bundle explicitly; the scripts do not resolve `ffmpeg` or `ffprobe` from
`PATH`:

```powershell
$ffmpegRoot = 'H:\Maelstrom Rust\.deps\ffmpeg-project-8.1'
& 'C:\Program Files\PowerShell\7\pwsh.exe' -NoProfile -File `
    'H:\Maelstrom Rust\scripts\Generate-MediaFixtures.ps1' -FfmpegRoot $ffmpegRoot
& 'C:\Program Files\PowerShell\7\pwsh.exe' -NoProfile -File `
    'H:\Maelstrom Rust\scripts\Test-MediaFixtures.ps1' -FfmpegRoot $ffmpegRoot
```

The short MP4 uses the same `testsrc2`/sine source class as the Windows packaging acceptance
clip. `vfr-irregular-mpeg4.mp4` is a separate generated five-frame, video-only MPEG-4 fixture:
it selects source frames at 0, 40, 110, 150, and 240 ms, producing deliberate 40/70/40/90 ms PTS
gaps with no B-frames. It is generated, not downloaded, and carries no third-party video or
audio content. The validator scans frame PTS against this exact contract; it does not infer VFR
from `r_frame_rate`.
The harness is deliberately independent of the editor executable: today it validates the media
contract that decode, waveform, and export tests can consume by absolute local path. It does not
silently make Cargo tests depend on generated binaries. Windows packaging continues to create its
own acceptance clip in `scripts/package-windows.ps1`; the matching source recipe keeps those two
paths comparable without coupling a release package to this local corpus.

For licensed, camera, device, or long-GOP samples, keep files outside Git and set an explicit
local corpus root before opting in:

```powershell
$env:MAELSTROM_REAL_MEDIA_ROOT = 'D:\Maelstrom-Real-Media'
& 'C:\Program Files\PowerShell\7\pwsh.exe' -NoProfile -File `
    'H:\Maelstrom Rust\scripts\Test-MediaFixtures.ps1' `
    -FfmpegRoot $ffmpegRoot -IncludeRealCorpus
```

The corpus hook only probes local files; it does not copy, upload, hash into the repository, or
assert a license. Record acquisition source, license/permission, and intended coverage alongside
the local corpus. Do not add any file without redistribution permission to Git.

`Test-MediaFixtures.ps1 -ManifestOnly` is the fast schema/path/uniqueness validation used when
FFmpeg is unavailable. The Phase 0 measurement gate remains incomplete: this harness establishes
only its generated-fixture and optional-corpus foundation.
