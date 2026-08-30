# Media fixtures

`fixtures/media/manifest.json` is the versioned contract for deterministic media used by the
local harness. The media binaries themselves are generated below `artifacts/media-fixtures`, which
is ignored by Git. Each manifest entry records its ID, recipe, provenance, SHA-256, byte size,
container/stream contract, duration, and whether probing must succeed or fail.
Schema version 3 adds manifest-controlled FFprobe `has_b_frames` reorder-delay values and exact
keyframe positions for every video fixture. VFR timing records presentation PTS as rounded integer
microseconds, required distinct timestamp gaps, and a SHA-256 fingerprint of the comma-separated
PTS sequence.
Optional `profile` and `pixel_format` contracts are also checked against FFprobe.

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
gaps with no B-frames. `vfr-reordered-mpeg2.ts` selects eight 24 fps source frames and encodes
MPEG-2 with two B-frames; its contract pins decoded presentation PTS, packet-order PTS, picture
types, and requires observable packet reordering. Both are generated, not downloaded, and carry
no third-party video or audio content. The validator scans the exact timing contract; it does not
infer VFR from `r_frame_rate`.
The two shifted 10-bit MOV fixtures add ProRes Standard and DNxHR HQX coverage,
eight all-intra frames with the same irregular 24 fps selection and a seven-second
presentation origin. Their manifest pins 10-bit 4:2:2 pixels and exact timestamps.
Independent CLI pixel/seek comparisons and app local-time mapping are described in
[codec/color qualification](codec-color-qualification.md).
The harness is deliberately independent of the editor executable. Normal Cargo tests do not depend
on generated binaries; the opt-in Phase 0 runner passes the generated reordered TS by its exact
absolute path through `MAELSTROM_REORDERED_VFR_TEST_MEDIA` to focused waveform, monitor-decode,
and app analysis/preview tests. Those tests gate local PTS origin normalization, exact local
presentation boundaries, and preview floor/hold spans for this one fixture. Windows packaging
continues to create its own acceptance clip in `scripts/package-windows.ps1`; the matching source
recipe keeps those two paths comparable without coupling a release package to this local corpus.

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
FFmpeg is unavailable. The Phase 0 measurement gate remains incomplete: it additionally gates the
reordered fixture's waveform/decode/preview source-time path and the two generated
10-bit MOV decode/preview paths, but does not prove export behavior or broad real-media coverage.
