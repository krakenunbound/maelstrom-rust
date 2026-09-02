# Hardware decode timing and native-resolution color parity

## Correction — 2026-08-31

Native-size H.264 preview conversion depended on decoder output layout. The
software decoder supplies planar YUV420P; Windows hardware transfer supplies
NV12. Both contain the same image, but the default unscaled RGB conversion paths
produce different chroma edges. On the first generated 1920x1080 BT.709 frame,
3,350,500 of 8,294,400 RGBA bytes differ, with maximum channel error 79/255.
Small 64x48 checks missed this: downscaled output matched exactly already.

Independent FFmpeg experiments isolate conversion from decoding: software decode
followed by lossless NV12 layout conversion produces exactly the hardware RGB
output. Enabling `accurate_rnd` instead makes planar and NV12 RGB output identical.
The app now adds `ACCURATE_RND` to its existing bicubic/bilinear scaler flags.
Resolution, selected filtering, source data, hardware selection, cache limits,
and UI scheduling remain unchanged. No additional conversion buffer or decoder
is introduced. This improves consistency; it is not a lower-resolution shortcut.

The [pinned FFmpeg 8.1 header](https://raw.githubusercontent.com/FFmpeg/FFmpeg/n8.1/libswscale/swscale.h)
documents accurate conversion flags and their optimization tradeoff. Exact
layout parity here is measured on the local runtime, not claimed for every CPU,
GPU, pixel format, or FFmpeg version. `BITEXACT` is not added, so this is not a
cross-platform bit-exact guarantee.

## Regression and reference contract

- A GPU-free regression creates equivalent row-stride-aware YUV420P/NV12 frames
  with patterned luma and sharp chroma edges. Native and downscaled outputs must
  match exactly in both quality modes. Removing only the production flag change
  reproduces the native-size failure; restoring it passes.
- Twelve opt-in tests explicitly open D3D11VA, DXVA2, NVIDIA CUVID, or Intel Quick
  Sync. Unsupported hardware, missing input, software fallback, or pixel/timestamp
  mismatch fails; none is counted as a software success.
- Each codec/backend runs 19 forward/reverse/repeated-final/fresh-seek cases at
  64x48 and 19 at 1920x1080: **456 exact comparisons** in the local matrix.
  D3D11VA/DXVA2 frames must increment hardware-transfer samples. Named CUVID/QSV
  decoders expose CPU-readable frames and must use that distinct production path.
  Every case retains the requested backend without fallback and preserves
  request/target identity.
  This real-hardware matrix uses high-quality bicubic; bilinear layout parity
  is covered by the separate GPU-free regression, not claimed as hardware proof.
- Independent sequential FFmpeg decoding supplies all RGBA bytes. H.264/HEVC use
  software decode; AV1 uses an explicitly selected opposite named hardware decoder
  (`av1_qsv` for CUVID actual, `av1_cuvid` for QSV/native actual), so the known
  failing default AV1 decoder is never mistaken for a reference.
  References explicitly use bicubic plus accurate conversion to describe the
  corrected color policy, not the former layout-dependent default. FFprobe
  supplies presentation timestamps; the hardware-test reference uses AV1 packet PTS because its frame
  best-effort timestamps are unavailable through the default decoder. Application analysis remains
  frame-derived by retrying explicit CUVID/QSV AV1 decoders. The existing one-microsecond rounding
  tolerance applies only to timestamps; pixel tolerance remains zero.
- Fixture bounds are eight frames, positive shifted origin, irregular increasing
  timestamps, 16:9 H.264/HEVC sources at most 1920x1080, plus the 352x288 AV1
  Main yuv420p AOM source. AV1 retains the 1920x1080 large-output gate. This is
  finite qualification, not arbitrary-file conformance.

The test helper uses the production native-device or named-decoder opener while
forcing the requested backend. D3D11VA and DXVA2 are backend APIs against the
default adapter; CUVID and QSV prove named decoder paths on this host. None proves
which physical adapter serviced a request on a multi-device system. VideoToolbox,
HDR, export parity, and broader source/reference-machine coverage remain open.
No editor launch or physical presentation measurement occurred.

## Reproduction

The bounded Phase 1 qualification wrapper reuses the three existing hardware VFR
fixtures; it never creates, downloads, or overwrites them. It runs six native
D3D11VA/DXVA2 and six named CUVID/QSV H.264/HEVC/AV1 ignored tests serially in
release mode. Before Rust tests, it decodes the AV1 source through D3D11VA,
DXVA2, CUVID, and QSV FFmpeg CLI paths to yuv420p `framemd5`; every path must
match the repeated official AOM sequence
`1998867ce2f47e15728862d6b55de0b4,48e9c8687a16b488ba1f7c49cb1f78fc` x4. Each
backend/codec must emit separate 64x48 and 1920x1080 `8 VFR boundaries, 19 exact
CLI-reference seek cases` evidence. A local mutex serializes invocations. For a
valid writable report destination, it independently attempts the capped log and
an atomic schema-versioned report on pass or operational/test failure; a log-write
failure is recorded in the report. Invalid or unwritable report destinations
cannot self-report. It does not launch the editor or any GUI.

```powershell
pwsh -NoProfile -File .\scripts\Run-Phase1HardwareVfr.ps1
```

The report and a UTF-8 capped-at-1,048,576-byte log are retained in ignored
`artifacts/phase1-hardware-vfr/`. Add `-IncludeAdapterInventory` only to record
an inventory-only list of adapters; it is not proof of physical-GPU coverage.
The report records source state, local FFmpeg identity, documented fixture hashes
and observed sizes, the twelve backend/codec results, AV1 CLI pixel-preflight evidence,
and `exact_cases_total: 456` only when all twelve tests meet the output contract. `authoritative` is true only for a pass
whose non-null start/end commits match and whose tracked source is clean at both
points; untracked and ignored evidence is deliberately excluded from that source
state. This is a reproducible harness contract, not a fresh authoritative
qualification claim by itself; the retained result is recorded below.

## Current authoritative runner checkpoint — 2026-09-01

The schema-1 runner passed from clean commit
`49653179fe15cb64a783c47c7543e1844ff94d75`. It verified all 42 files in the
project-built FFmpeg checksum inventory and passed all twelve backend/codec tests:
D3D11VA, DXVA2, NVIDIA CUVID, and Intel Quick Sync against H.264 High, HEVC
Main 10, and AV1 Main. Every path passed 19 forward/reverse/repeated-final/fresh
seeks at both 64x48 and 1920x1080, for 456 exact timestamp-and-pixel comparisons
with no fallback. Before the Rust matrix, all four FFmpeg CLI paths decoded the
AV1 fixture to the exact alternating frame MD5 sequence published by AOM.

The AV1 fixture is a local-only, deterministic stream-copy derivative of the
pinned two-frame AOM vector. Its ignored MKV is 516,187 bytes with SHA-256
`6ADB3B081701F13ED7C5EFDC26F092E08D474AE2D9E7840B6C58A2B937A9EC9C` and
retains eight packets at a five-second source origin with irregular gaps. The
repository records provenance, input/output hashes, exact packet timing, and the
preparation recipe without redistributing either AOM input or the derivative.

Each QSV output size again recorded exactly seven named-decoder reopens. H.264
measured 18.216 ms mean / 25.900 ms max at 64x48 and 23.915 ms mean / 35.231 ms
max at 1920x1080. HEVC measured 19.981 ms mean / 27.566 ms max at 64x48 and
26.472 ms mean / 30.726 ms max at 1920x1080. AV1 measured 17.468 ms mean /
28.736 ms max at 64x48 and 16.470 ms mean / 24.259 ms max at 1920x1080. These
host-specific synchronous decoder-recreation spans are diagnostic evidence, not
GPU execution, end-to-end scrub latency, or a universal no-lag threshold.

The retained local report is 12,612 bytes with SHA-256
`61F70C226FC992B4770F66E77331F6F4728CCF5BC7D79622E3307ADFC2AB6658`; its
8,761-byte log SHA-256 is
`CF271A1059DC7336BADDC2603C4E28CDE7480DFFF28FF3975D93EFAA7FDCFEEC`.
Optional inventory listed NVIDIA GeForce RTX 3090 and Intel UHD Graphics 770,
but remains inventory rather than physical-adapter attribution. No editor, GUI
surface, export path, or physical scanout was exercised.

## Prior authoritative runner checkpoint — 2026-09-01

The expanded schema-1 runner passed from clean commit
`e7cbf0ce42a5e4f2f624d00efc76f7b5fc9c2bca`. It verified all 42 files in the
project-built FFmpeg checksum inventory and passed all eight backend/codec tests:
D3D11VA, DXVA2, NVIDIA CUVID, and Intel Quick Sync against H.264 High and HEVC
Main 10, each at 64x48 and native 1920x1080. The result contains 304 exact
timestamp-and-pixel comparisons and rejects software fallback.

The first QSV qualification exposed a reverse-only correctness defect: after a
decoder flush, QSV could report the requested timestamp while retaining pixels
from the preceding surface. A fresh monitor at the same target and sequential
QSV decoding were exact. Maelstrom now supplies the packet time base and reopens
only the named QSV decoder on backward seeks, clearing its asynchronous surface
queue. Both codecs now pass the complete forward/reverse/repeated-final/fresh
matrix. Native Windows and CUVID keep the cheaper proven flush path. The report
now measures the synchronous named-decoder recreation boundary separately from
ordinary decoder calls, including failed attempts. Each QSV output size recorded
exactly seven reopens. H.264 measured 17.007 ms mean / 19.476 ms max at 64x48 and
15.309 ms mean / 17.898 ms max at 1920x1080. HEVC measured 18.610 ms mean /
21.974 ms max at 64x48 and 17.543 ms mean / 23.146 ms max at 1920x1080. These
host-specific CPU elapsed spans are diagnostic evidence, not GPU execution,
end-to-end scrub latency, or a universal no-lag threshold.

The retained local report is 7,700 bytes with SHA-256
`A8E88B66197F508767ED84644F9887FE211F4499E3310ECF7CB62DBC1426BAB5`; its
5,856-byte log SHA-256 is
`CC48FA48AE6B92A5EE192DDE96192D67797DC7103C9687E67BAD0D78667E15E3`.
Optional inventory listed NVIDIA GeForce RTX 3090 and Intel UHD Graphics 770,
but remains inventory rather than physical-adapter attribution. No editor, GUI
surface, export path, or physical scanout was exercised.

## Earlier authoritative runner checkpoint — 2026-08-31

The schema-1 runner passed from clean commit
`a84838e4a708babcd9346b7ac969aab42969f866`. It verified all 42 files in the
project-built FFmpeg checksum inventory and passed all four backend/codec tests:
D3D11VA and DXVA2 against H.264 High and HEVC Main 10, each at 64x48 and native
1920x1080. The result contains 152 exact timestamp-and-pixel comparisons, with
hardware transfer required and software fallback rejected by the tests.

The retained local report SHA-256 is
`D97439E5AC4821F7B11E05D52BEE5EEEE6B45886F76B1364E876DEF89452737B`; its
2,684-byte log SHA-256 is
`C64EAF6042AFF8DF4F164C079974B70D08AB5E570812BEE33D68410C3EB3EFF9`.
The optional inventory listed Intel UHD Graphics 770 and NVIDIA GeForce RTX 3090,
but the backend APIs use the system default adapter. That list is not evidence
that both physical GPUs decoded the clips. No editor, GUI surface, export path, or
physical scanout was exercised.

Use the workspace's approved FFmpeg bundle. The generated test patterns require
no third-party media or model downloads. Run from `H:\Maelstrom Rust`:

```powershell
$ffmpeg = 'H:\Maelstrom Rust\.deps\ffmpeg-project-8.1\bin\ffmpeg.exe'
$artifacts = 'H:\Maelstrom Rust\artifacts\phase1-multisource'
$selected = "select='eq(n,0)+eq(n,1)+eq(n,3)+eq(n,4)+eq(n,6)+eq(n,8)+eq(n,11)+eq(n,12)'"
& $ffmpeg -v error -n -f lavfi -i 'testsrc2=size=1920x1080:rate=24' `
  -vf "$selected,setpts=PTS+7/TB" -frames:v 8 -fps_mode vfr -an `
  -c:v h264_qsv -global_quality 20 -g 8 -bf 2 -color_range tv `
  -colorspace bt709 -color_primaries bt709 -color_trc bt709 -map_metadata -1 `
  "$artifacts\codec-vfr-h264-bt709-1080p-hardware.mp4"
& $ffmpeg -v error -n -f lavfi -i 'testsrc2=size=1920x1080:rate=24' `
  -vf "$selected,format=p010le,setpts=PTS+7/TB" -frames:v 8 -fps_mode vfr -an `
  -c:v hevc_qsv -profile:v main10 -global_quality 20 -g 8 -bf 2 -color_range tv `
  -colorspace bt709 -color_primaries bt709 -color_trc bt709 -map_metadata -1 `
  "$artifacts\codec-vfr-hevc-main10-bt709-1080p-hardware.mp4"
$env:FFMPEG_DIR = 'H:\Maelstrom Rust\.deps\ffmpeg-project-8.1'
$env:LIBCLANG_PATH = 'H:\Maelstrom Rust\.deps\libclang-bindgen'
$env:PATH = "$env:FFMPEG_DIR\bin;$env:LIBCLANG_PATH;$env:PATH"
$env:MAELSTROM_HARDWARE_H264_VFR_TEST_MEDIA = "$artifacts\codec-vfr-h264-bt709-1080p-hardware.mp4"
$env:MAELSTROM_HEVC_VFR_TEST_MEDIA = "$artifacts\codec-vfr-hevc-main10-bt709-1080p-hardware.mp4"
& 'C:\Users\The Kraken\.cargo\bin\cargo.exe' test -p nle-decode --release supplied_windows_ -- --ignored --test-threads=1 --nocapture
& 'C:\Users\The Kraken\.cargo\bin\cargo.exe' test -p nle-decode --release scaler_layout -- --include-ignored --test-threads=1 --nocapture
```

Use a dedicated shell for these environment settings. `-n` preserves existing
fixtures; reuse them or choose new filenames rather than overwriting evidence.
QSV encoding requires compatible local hardware and is not a deterministic public
fixture-manifest contract. The actual muxed timestamps are read, not assumed from
the input selection recipe. The local H.264/HEVC files contain B pictures and a
seven-second origin; their hashes are:

| Fixture | SHA-256 |
|---|---|
| H.264 High, 8-bit | `503B39F6C101F8395B49AD424711357DC317C2CAEFCFCC9E5F795A0D46CDCAA6` |
| HEVC Main 10 | `1AF892D8C40634E354A05FD80A446298C5501914D2E39ECD641D792B6538C486` |

## Verification boundary

The current release workspace passes 861 tests, with 36 opt-in tests ignored. The twelve
hardware tests were run separately through the bounded qualification harness, not inferred
from ignored results. Strict all-target workspace Clippy and formatting pass.
All seven primary deterministic fixture contracts, the separate local AV1 derivative contract,
and all seven Phase 0 scenarios pass.
The missing-required-input negative control fails as intended rather than
reporting an empty hardware test as successful.
The independent four-source Full-1080p app gate passes with 161 microseconds
submission, 62 ms until all frames, five peak sessions under eight, and zero
sessions after teardown. These are headless worker measurements, not scanout.

At this hardware-parity checkpoint, the older isolated latency-comparison test failed because no decoder
backend was observed despite retained frames being present. Paused prewarm
workers can populate the shared cache before the foreground reply; cached frames
intentionally omit backend provenance. The probe only waits for retained frames,
not a provenance-bearing decode. Its failure is preserved in
`hardware-parity-latency.log`; no passing latency report or windowed qualification
is claimed from that run. Fixing the measurement without inventing cache
provenance was left as a separate task. The subsequent bounded successful-work observation
repair and three passing 40-trial runs are documented in `docs/phase1-latency-comparison.md`;
they do not retroactively qualify this checkpoint's failed run or prove windowed playback.

The accurate scaler has a measurable CPU cost. The diagnostic measures only the
existing scaler timing span, excluding hardware transfer, demux/decode, RGBA
packing, GPU upload, and display. It is not a throughput/real-time acceptance gate.
Each timing run uses eight warmups and 120 measured conversions per layout/filter:

| Native 1080p conversion | Original p50 / p95, ms | Corrected p50 range / p95 range, ms (three runs) |
|---|---|---|
| Planar, bilinear | 1.050 / 1.135 | 2.694–2.721 / 2.814–2.959 |
| NV12, bilinear | 2.859 / 2.963 | 2.767–2.779 / 2.920–3.128 |
| Planar, bicubic | 1.146 / 1.263 | 4.721–4.776 / 4.917–4.995 |
| NV12, bicubic | 2.869 / 2.978 | 4.800–4.818 / 5.057–11.299 |

The NV12 tail outlier is retained, not retried away. The earlier corrected
exploratory run also had an 11.680 ms p95. Accurate conversion increases worker
CPU cost; optimization and sustained/windowed requalification remain necessary
before any no-lag claim. The user-selected bilinear/bicubic distinction stays
intact; the fix does not quietly choose the cheaper filter.

Before/after logs, failed assertions, generated media, and independent raw output
are retained under ignored `artifacts/phase1-multisource/` with prefix
`hardware-parity-`. Broader playback/latency and preview/export color gates remain
open.

## Packaged checkpoint

The 36,141,568-byte portable executable matches the release build, SHA-256
`8F0965B4489D34A11BEB5D7279DEF9DB8766AB611061EF7C119FAC793398A79A`.
The package was built with `-SkipSmoke`; its status is explicitly `not_run`.
The full-path launcher runtime check and packaged FFmpeg/FFprobe loader checks
pass. No editor was launched, and historical windowed evidence is not relabeled
as evidence for this new executable. Independent review found no blocking defect.

The local evidence manifest is `hardware-parity-verification.json`, SHA-256
`EE6610FEF4C3CD3E322ABBA37FED5921A769C4BE452F0454A8A19135557BD10B`.
It binds fixture, log, Phase 0 report, and package hashes, records the failed
latency probe separately, and confirms no editor/compiler/test/media process
remained at verification.
