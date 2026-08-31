# Single-allocation monitor RGBA packing

Local Windows checkpoint, 2026-08-31. This reduces measured CPU frame-packing
work without reducing resolution or changing pixels. It does not establish
lag-free playback, sustained display, or cross-machine performance.

## Mechanism and ownership

The old path copied scaled RGBA rows into a zeroed `Vec`, optionally copied that
into a second letterboxed `Vec`, then allocated and copied again into `Arc<[u8]>`.
The new path initializes the final shared allocation directly. It still copies
pixels out of FFmpeg-owned storage; this is not zero-copy decoding.

Native rows initialize every output byte. Letterboxed output is initialized to
transparent black before active rows are copied, retaining the same floor-centered
placement (odd extra pixels on the right/bottom). RGBA values including alpha are
unchanged; input row padding is discarded. The decoder/cache/UI contract remains
an immutable, owned `Arc<[u8]>`, with no FFmpeg storage lifetime escaping.

The private row helper checks nonzero dimensions, the existing 4096-dimension and
64 MiB frame limits, scaled-to-output bounds, stride, checked lengths, and input
slice coverage before allocating. The Video adapter additionally validates format,
shape, positive stride, and plane range against its reference-counted allocation
before ffmpeg-next constructs a slice. Scaler output from either backend is backed
by `av_frame_get_buffer`. Review caught the initially missing positive-overlarge
stride check; a regression now exercises it alongside negative/zero strides.

The small unsafe initialization region only writes into a fresh, exclusively
owned allocation and exposes it after every byte is initialized. No new dependency,
SDK, model, FFmpeg version, scaler policy, filter flag, color conversion, cache
budget, project schema, or GUI control is introduced. Rust 1.92 supports the APIs;
the [pinned Rust implementation](https://github.com/rust-lang/rust/blob/1.92.0/library/alloc/src/sync.rs)
documents allocation and deferred initialization of shared slices.

## Measurement boundary correction

`rgba_copy_letterbox` now includes the final shared allocation and filling it.
Previously the final `Vec`-to-`Arc` allocation/copy happened outside that timer.
Historical per-stage reports therefore undercount that old packing path and must
not be compared directly with the corrected span. Whole-request timing is unchanged.
The paired diagnostic below measures the complete old and new operations, not
those historical stage counters.

## Exact-pixel and regression proof

- Eleven focused tests pass. They include 1,296 exact legacy comparisons across
  small native/odd/padded layouts, five native/letterboxed full-resolution cases
  including 4K, source mutation/drop independence, Arc sharing, malformed lengths,
  stride poison, alpha, transparent padding, and request/tick identity.
- The complete release workspace passes 764 tests, with 23 opt-in tests ignored.
  This includes the existing 400 exact legacy-converter comparisons.
- Four explicitly enabled Windows D3D11VA/DXVA2 H.264/HEVC Main 10 tests pass
  152 independent CLI-reference pixel/timing comparisons at 64x48 and 1920x1080.
- Strict release workspace/all-target Clippy, formatting, and independent review
  pass on the final source.

The first focused run failed only in a new test's setup: it omitted the retained
software scaler required by the production contract. The corrected test initializes
that scaler. Both failure and subsequent passing logs are preserved locally.

## Local packing diagnostic

Intel Core i7-14700K, pinned project FFmpeg 8.1, release build. Each implementation
gets eight warmups and 120 samples per case. Timing includes allocation, row copy,
letterbox initialization, and conversion to shared ownership where needed. It
excludes output destruction, pixel assertions, decoding, scaling, upload, and
presentation. Every sampled output is checked byte-for-byte against the unchanged
legacy test oracle. Percentiles use nearest rank.

Milliseconds, measured after the final source change:

| Scaled frame -> output | Legacy p50 / p95 | Direct packing p50 / p95 |
|---|---:|---:|
| 1920x1080 -> same | 2.658 / 2.941 | 1.363 / 1.503 |
| 810x1080 -> 1920x1080 | 4.061 / 4.549 | 1.444 / 1.555 |
| 1920x810 -> 1920x1080 | 3.633 / 3.840 | 1.689 / 2.015 |
| 1919x1079 -> 1920x1080 | 4.332 / 4.840 | 1.829 / 2.145 |
| 3840x2160 -> same | 11.659 / 12.260 | 5.820 / 6.176 |

Legacy runs before production in each pair; allocator/thermal ordering can bias
the comparison. These are local diagnostic observations, not a stable speedup
guarantee or a passed UI deadline. The log retains percentile summaries rather
than individual packing timing samples.

## Application latency and remaining gates

Three runs of the existing paused/prewarmed Full-1080p latency workload retain
all forty interleaved one/four-source trials, unchanged fixture bytes, and the
original committed report assertions. A local adapter skips fixture regeneration
and chooses distinct report filenames; it executes the runner's read-only
validation statements unchanged, additionally checking active/peak session caps.

| Run | One-source submit p95 (us) | Four-source submit p95 (us) | One-source ready p95 (ms) | Four-source ready p95 (ms) |
|---|---:|---:|---:|---:|
| 1 | 98 | 156 | 38 | 64 |
| 2 | 119 | 170 | 42 | 67 |
| 3 | 127 | 125 | 40 | 63 |

All 120 trials preserve Full raster size, source IDs/ticks, observed backend
evidence, the eight-session cap, and zero post-drop sessions. Raw report samples,
distributions, ordering, deltas, and ratios validate. Backend was Software.
Prior four-source ready p95 was 63 ms: **these results do not demonstrate an
end-to-end scrub-latency improvement** despite the lower isolated packing cost.
Five-millisecond polling and host variation limit these comparisons.

The prior threaded-scaler soak did not qualify this changed source. A subsequent
run at `e0f35d155c42af307581d216e6798127e4e8d43c` now passes the local ten-minute
cache/session resource gate: 600.034 seconds, 21,072 four-source cycles, zero
errors, bounded memory/sessions, and zero post-drop sessions. See
`phase1-sustained-soak.md` for raw evidence and limitations. Windowed playback,
lower-core contention, physical input/scanout, native audio under this workload,
and broader hardware qualification remain open. GUI launch permission is pending.

## Evidence and local package

Ignored evidence is under `artifacts/phase1-multisource/`: `rgba-packing-*-final.log`,
`rgba-packing-latency-{1,2,3}.{json,log}`, `run-rgba-packing-latency.ps1`, and
`rgba-packing-package.log`. `rgba-packing-verification.json` hashes code, fixtures,
runtime configuration, tests, reports, and the package as a checkpoint snapshot,
not a signed execution attestation.
All 36 entries were rehashed successfully. Manifest SHA-256:
`7DE730249C9783684C156A2BBE6BD782A6A583BF08921FB2A1D6A6983ED0A646`.

The portable package was rebuilt without launching the editor. Its executable
matches the release build, SHA-256:
`4715E17526744CC803748660E8BD978FA8C2917E682435E88E15146A3006F49A`.
The exact approved launcher passes `--verify-runtime`. Packaged FFmpeg/FFprobe
load with package/Windows paths only; eleven FFmpeg DLLs and the authorized Visual
Studio runtime match their sources. `PACKAGE-STATUS.json` remains `not_run`.

The generated package was replaced; its previous executable/status remain in
`rgba-packing-previous-package.zip`. All three historical smoke reports retain
their original hashes and are not treated as qualification of this package.
