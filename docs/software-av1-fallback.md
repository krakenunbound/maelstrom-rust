# Software AV1 fallback

## Contract

Maelstrom keeps full selected preview resolution when hardware AV1 decode is unavailable. It does not
claim a hardware backend or silently lower image quality. The monitor tries the existing Windows,
NVIDIA CUVID, and Intel Quick Sync candidates first. It then explicitly opens `libaom-av1` and reports
the Software backend. Waveform frame-timing analysis and export input decoding use the same named
fallback instead of relying on FFmpeg's native `av1` decoder selection.

## Reproducible runtime

`scripts/build-ffmpeg-lgpl-windows.ps1` drives the WSL recipe and preserves the installed bundle until a
complete staged replacement validates. The recipe pins:

- AOMedia libaom tag `v3.13.0`, peeled commit
  `d9c115ce0951324dee243041ef810e27202de20f`;
- decoder enabled, encoder/tests/tools/examples/docs disabled;
- x86-64 static build with no libaom DLL;
- FFmpeg `--enable-libaom --disable-encoder=libaom_av1` and the MinGW pthread link required by the
  static decoder.

The bundle carries `libaom-LICENSE.txt`, `libaom-PATENTS.txt`, the exact source/configuration manifest,
and hashes for runtime/link artifacts. Packaging and hardware qualification require those records,
verify `libaom-av1` exists only in the decoder inventory, and reject `libaom*.dll`. No model, codec
binary, or separately downloaded DLL is committed to the public repository.

## Evidence

The local-only AOM fixtures are an eight-frame AV1 Main/yuv420p stream in Matroska and WebM, with a
five-second source origin and irregular local PTS
`0/33/100/133/200/267/367/400 ms`. For each container, the forced-software regression:

- requires the named `libaom-av1` decoder and `DecodeBackend::Software` with `ForcedSoftware` reason;
- compares all RGBA bytes and timestamps with an independent FFmpeg `-c:v libaom-av1` sequential
  decode;
- exercises eight forward, eight reverse, one repeated-final, and two fresh-monitor seeks;
- limits each monitored seek to at most 24 demux packets.

Both 19-case tests pass. The same runtime also passes AV1 waveform presentation timing, app
floor/hold preview routing, save/reopen frame-index reconstruction, Matroska/WebM export source
identity, the complete Phase 0 matrix, and the serial release workspace (914 passed, 38 ignored).
Because libaom registration can become FFmpeg's decoder-by-ID result, Windows hardware-context decode
explicitly selects the native `av1` decoder; the fresh local hardware rerun passes all 12 backend/codec
tests and 456 exact cases. That dirty-tree report is validation evidence, not an authoritative clean-commit
checkpoint. A fresh `-SkipSmoke` Windows package contains the named decoder and both notices, contains
neither the encoder nor a libaom DLL, and passes the exact full-path launcher `--verify-runtime` check.
Because smoke was deliberately skipped, that package is locally complete but not GUI-qualified. The
editor was not launched.

This finite gate does not establish arbitrary-file AV1 conformance, sustained 1080p software playback,
windowed latency, display scanout, or cross-machine performance. Those remain separate qualification
work.
