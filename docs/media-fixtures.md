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

The generated audio set now covers mono, stereo, and multichannel layouts. The 5.1 PCM WAV carries
six distinct one-second tones in standard order (FL 220 Hz, FR 330 Hz, FC 440 Hz, LFE 55 Hz,
BL 550 Hz, BR 660 Hz), so future decode, mixer, routing, and export checks can distinguish channels
instead of treating a silent layout tag as evidence. The manifest pins its six-channel `5.1`
FFprobe identity, exact byte size, and SHA-256. This is corpus coverage only; it does not claim that
the current mixer or exporter preserves every multichannel layout.
`vfr-reordered-shifted-mpeg4.mp4` combines the same eight-frame selection at 320×180/30 fps with
a three-second presentation origin and MPEG-4 B-frames. Its MP4 contract pins Advanced Simple
Profile/yuv420p, decoded presentation PTS from 3.000000 to 3.400000 seconds, packet-order PTS,
picture types, and observable reordering. It is the compact source-time fixture for code that must
distinguish presentation origin from local clip time.
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
FFmpeg is unavailable. The Phase 0 measurement gate remains incomplete: it gates the reordered
fixture's waveform/decode/preview source-time path, the two generated 10-bit MOV paths, and
source-identity export across those fixtures plus the local AV1 fixture. It does not prove broad
real-media, display, or cross-machine coverage.

## Local AOM AV1 shifted-VFR fixture

`fixtures/media/av1-vfr-fixture.json` is a separate, reproducible contract for a local-only AV1
fixture. It records AOM checksum manifest commit `a40ed1ea9e4ecc3df58a5bccb76623f2c94ae727`
as immutable provenance and pins the official 39-frame
`av1-1-b8-02-allintra.ivf` test vector and MD5 manifest by URL, SHA-1, SHA-256, and size. The first
eight published frame MD5 values are distinct. The derived `vfr-av1-aom-shifted.mkv` is 306,777
bytes with SHA-256 `B8C8092F924CFC743510A3CCC6EEFB627854963D0232497DBB413FCA177ECD21`.
The script stream-copies the first eight all-intra frames, then uses `setts`
(`time_base=1/1000`, `prescale=1`) to assign PTS/DTS of 5000, 5033, 5100, 5133, 5200, 5267, 5367,
and 5400 ms. It does not decode and does not need a GPU.

The AOM inputs and resulting MKV stay only in ignored `artifacts/media-fixtures`; this repository
does not redistribute them and makes no licensing claim. Obtain and use the source under its own
terms.

The opt-in `MAELSTROM_AV1_VFR_TEST_MEDIA` waveform and app checks keep timing decoder-derived:
the normal bounded decoded-frame scan runs first, then AV1 alone retries the production named-decoder
order (`av1_cuvid`, then `av1_qsv`) when the bundled default decoder produces no usable frames.
No packet-to-frame assumption is used. The resulting local presentation index is
0/33/100/133/200/267/367/400 ms, and app preview routing is checked at every boundary in both
directions. The export gate also checks head, trim, slip, exclusive-tail, and final-frame identity at
30/1 and 30000/1001. This proves the local fixture and usable decoder paths on this host, not
arbitrary AV1 conformance or physical-adapter identity.

With already acquired local inputs, run:

```powershell
& 'C:\Program Files\PowerShell\7\pwsh.exe' -NoProfile -File `
    'H:\Maelstrom Rust\scripts\Prepare-Av1VfrFixture.ps1'
```

To fetch the pinned inputs explicitly from `https://storage.googleapis.com/aom-test-data/`, add
`-DownloadInputs`:

```powershell
& 'C:\Program Files\PowerShell\7\pwsh.exe' -NoProfile -File `
    'H:\Maelstrom Rust\scripts\Prepare-Av1VfrFixture.ps1' -DownloadInputs
```

The preparation script accepts no FFmpeg override and requires exactly
`H:\Maelstrom Rust\.deps\ffmpeg-project-8.1` (FFmpeg 8.1). Downloads and the
derived output are verified at temporary paths and moved into place only after the complete
contract passes, so a failed fetch or remux does not replace a known-good local artifact.
