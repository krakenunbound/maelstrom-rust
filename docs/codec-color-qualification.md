# Ten-bit codec timing and preview color conversion

Follow-up: [shifted VFR export source identity](shifted-vfr-export-parity.md) adds
20 ProRes/DNxHR export-graph timing cases; it does not qualify export color fidelity.

Follow-up: [hardware/native-resolution parity](hardware-decode-parity.md) adds
explicit Windows hardware proof and fixes a separate planar/NV12 conversion
inconsistency. Current reference filters include `accurate_rnd`; the checkpoint
and package hashes below describe the earlier 2026-08-30 matrix/range fix.

Independent sequential decoding exposed a DNxHR preview color error: the decoded
frames declare BT.709, but the monitor scaler used its default BT.601 matrix.
Preview conversion now reads each decoded frame's matrix and range before scaling.
The same fix applies to software frames and to frames transferred from hardware;
hardware transfer retains the original frame's metadata as the authority.

## Cause and regression evidence

The old DNxHR first frame differs from the independent FFmpeg CLI reference in
4,644 of 12,288 RGBA bytes, with a maximum channel difference of 41. Its pixels
match a CLI conversion forced to BT.601 exactly, while the source declares BT.709
and limited range. This isolates the matrix error from seeking, chroma sampling,
and frame identity. Removing only the new software color-configuration call
reproduces the original test failure; restoring it passes.

`ScalingContext::get` calls `sws_getContext`, whose default coefficients are
BT.601. The conversion now supplies `sws_setColorspaceDetails` with the declared
matrix/range, full-range RGBA output, and neutral brightness/contrast/saturation.
The pinned FFmpeg header and [FFmpeg's scaling API documentation](https://ffmpeg.org/doxygen/8.0/group__libsws.html)
define this matrix/range configuration. No UI-thread work, new decoder session,
dependency, quality reduction, or cache-budget change was introduced.

Configuration occurs inside the existing scaler timing span before each frame.
Tests compare a retained scaler against independent CLI output while switching
between BT.709 limited, BT.601 limited, BT.709 full, unspecified defaults, and back
to BT.709 limited. Separate checks preserve untagged YUVJ full range and unchanged
RGB/alpha pixels. Unspecified ordinary YUV input retains the previous BT.601
limited-range default rather than inheriting a preceding frame's settings.

This is matrix/range conversion, not a complete color-management pipeline. Transfer
functions, HDR tone mapping, BT.2020 constant-luminance, ICtCp, and broader
preview/export color parity remain unqualified.

## Real-media coverage

| Fixture | Pixel format | Timing | Exact reference checks |
|---|---|---|---:|
| Generated ProRes Standard MOV | 10-bit 4:2:2 | Eight irregular frames, 7 s origin | 19 |
| Generated DNxHR HQX MOV | 10-bit 4:2:2 | Eight irregular frames, 7 s origin | 19 |
| Supplied generated HEVC Main 10 MP4 | 10-bit 4:2:0, B pictures | Eight irregular frames, 7 s origin | 19 |

Every case checks forward and reverse boundaries, final-frame access, and fresh
middle/late seeks. Expected pixels come from a separate FFmpeg CLI sequential
software decode, bicubic scaling to 64×36, and transparent padding to 64×48;
they do not call the monitor's seek or packing helpers. Complete RGBA buffers must
match exactly. FFprobe supplies independent presentation timestamps; the existing
one-microsecond rational-rounding tolerance applies only to timestamp comparisons.
Each result must retain its requested target/request identity and Software backend.
The supplied contract requires eight declared frames, and the timestamp scan is
limited to 32 packets.

The deterministic MOV recipes, hashes, profile, pixel format, keyframe positions,
picture types, and presentation-PTS fingerprint are now in the seven-fixture
manifest. Both normalize to local boundaries
`0, 41667, 125000, 166667, 250000, 333333, 458333, 500000` microseconds.
The app regression runs normal background-analysis result handling and checks
preview addressing at each boundary and immediately before the next, in both
directions. It preserves local frame durations and does not invent a CFR duration
for the final frame. `Run-Phase0Scenarios.ps1` includes these generated decode and
app checks and restores their environment variables on exit.

The HEVC artifact was created locally with the approved bundle's `hevc_qsv`,
Main 10, `-g 8 -bf 2`, and the same shifted selected-frame source. Its actual muxed
presentation times differ from the selection recipe, so the test reads the file's
PTS rather than assuming encoder output timing. It is opt-in through
`MAELSTROM_HEVC_VFR_TEST_MEDIA`; hardware encoding does not imply hardware-decoding
qualification. The artifact's SHA-256 is
`02D292BB86641B3BC9A8B23E111670B3477D9D83BB9480027362F500010B10E2`.
No third-party content or additional media runtime was downloaded.

## Verified checkpoint — 2026-08-30

The release workspace passes 743 tests (16 opt-in tests ignored), with the supplied
HEVC, H.264, reordered MPEG-2, ProRes, and DNxHR fixture variables enabled. Strict
all-target release Clippy, formatting, all seven fixture contracts, the expanded
Phase 0 runner and its seven scenarios, and independent review pass.

The rebuilt 36,141,568-byte portable package matches the release executable,
SHA-256 `F44F08C87FDFD151268B2811ABC0E33DABA45407F6F342F6B76C50C0EE4D27DB`.
It was assembled with `-SkipSmoke`; general smoke status remains `not_run` and no
editor was launched for this checkpoint. The preceding windowed-qualified package
is preserved as `verified-input-Maelstrom.exe` with its package status in
`windowed-1e11e7cd-3285-4104-8884-4c1f4a1b6f9d`.

`codec-color-verification.json` records log/fixture/package/scenario hashes, the
original color mismatch, HEVC packet-reordering evidence, and an empty remaining
editor/compiler/media-tool process list. Its SHA-256 is
`80E4BBC5DE3B01A7E70F64DF0166F5C8D260F7B236EA0A46AF658DC43F61D557`.

Artifacts and before/after logs are under `artifacts/phase1-multisource/`; the two
deterministic MOV files live under `artifacts/media-fixtures/`. This coverage is
small-frame, local Software decoding. It does not establish Full-1080p/4K playback
speed, AV1 support, broader camera files, hardware pixel parity, export parity,
physical presentation latency, or completion of the cross-hardware roadmap gates.
