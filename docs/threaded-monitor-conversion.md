# Bounded full-resolution monitor conversion

Local Windows checkpoint, 2026-08-31. This repairs measured conversion overhead;
it does not close sustained/windowed playback or cross-machine performance gates.

## Mechanism and constraints

Accurate conversion is required: removing `ACCURATE_RND` previously produced
different planar/NV12 pixels. Neither that flag, the selected bilinear/bicubic
filter, output resolution, per-frame color configuration, nor cache identity changes.

The previous ffmpeg-next wrapper uses `sws_scale`, which processes one slice even
when the context has multiple slice contexts. The private monitor wrapper now
allocates and initializes a legacy context once with an explicit two-thread limit,
then uses `sws_scale_frame` to dispatch its slices. It deliberately does not use
FFmpeg's dynamically configured frame-conversion mode.

Policy: both input and output must contain at least 1920x1080 pixels, and Rust must
report at least eight available logical CPUs. Smaller/low-core cases retain the
old serial wrapper. Threaded setup failure also retains that exact serial path;
it never changes quality. Context initialization and conversion remain on decoder
workers. Fresh output allocation is checked; preallocated output is validated and
made writable. RAII releases partially initialized contexts and their workers.

Two is a per-context cap, not a global CPU reservation. Four foreground sources
can use two conversion threads each at the CPU floor, but speculative decoding,
codec frame threads, and up to eight retained sessions can still cause contention.
Lower-core and sustained workloads need further qualification; no universal gain
is claimed. No new model, SDK, DLL, dependency, runtime upgrade, or UI control is added.

Implementation references: pinned FFmpeg 8.1
[frame and slice dispatch](https://github.com/FFmpeg/FFmpeg/blob/9047fa1b084f76b1b4d065af2d743df1b40dfb56/libswscale/swscale.c#L1239),
[slice worker lifecycle](https://github.com/FFmpeg/FFmpeg/blob/9047fa1b084f76b1b4d065af2d743df1b40dfb56/libavutil/slicethread.c#L91).

## Pixel and lifecycle proof

- 400 exact comparisons against the actual previous ffmpeg-next single-slice
  converter, not another instance of the new wrapper: ten layouts (planar and
  semiplanar 8/10-bit YUV, 10-bit 4:2:2/4:4:4, YUVA, RGBA, BGRA, RGB24), five size
  pairs (native 1080p, odd native, odd resampling, small odd downscale, 4K-to-1080p),
  both filters, and four successive matrix/range states on each retained context.
  Tests force two threads, reject setup fallback, and query the initialized FFmpeg
  thread option. Every active output byte must match.
- Five policy/lifecycle tests cover CPU/area boundaries, actual thread caps,
  mismatched input/output followed by successful reuse, copy-on-write output
  isolation, and failed initialization followed by successful context creation.
- Four explicit D3D11VA/DXVA2 H.264/HEVC Main 10 tests pass 152 independent CLI
  pixel/timing comparisons, including native 1080p hardware-transferred frames.
  These use the default hardware adapter; they do not establish every GPU's parity.
- 753 release workspace tests pass; 22 opt-in diagnostics/gates remain ignored.
  Strict release workspace/all-target Clippy, formatting, and independent review pass.

## Local measurements

Intel Core i7-14700K, 20 cores / 28 logical CPUs; pinned project FFmpeg 8.1.
The unchanged production-path diagnostic uses eight warmups and 120 measured
conversions per row, with fresh RGBA allocation and per-frame color configuration.
The scaler span excludes decode, hardware transfer, RGBA row packing, upload, and
presentation. Figures are milliseconds; diagnostic percentile calculation is
`sorted[(n-1)*percent/100]`, not the latency gate's nearest-rank calculation.

| Layout / filter | Before p50 / p95 | Threaded p50 / p95 |
|---|---:|---:|
| YUV420P / bilinear | 2.895 / 3.217 | 1.631 / 1.849 |
| NV12 / bilinear | 2.982 / 3.322 | 1.665 / 1.852 |
| YUV420P / bicubic | 5.130 / 5.436 | 2.750 / 2.929 |
| NV12 / bicubic | 5.239 / 5.651 | 2.799 / 3.192 |

A separate release prototype compared 1/2/4 threads, with eight warmups and 80
samples per configuration. Bicubic p50 was 5.25-5.29 / 2.78-2.80 / 1.58-1.59 ms.
Two threads were chosen conservatively to limit multi-source contention, not
because four threads failed pixel parity. Thread startup is outside the retained
conversion span; prototype initialization was approximately 0.5-0.6 ms for two
threads. Its allocation/configuration/conversion/row-copy p50 was 4.21-4.23 ms,
versus 6.71-6.72 ms with one thread. These are diagnostic samples, not deadlines.

The existing paused, normally prewarmed Full-1080p latency gate also passed three
repetitions of 20 one-source and 20 four-source trials:

| Run | One-source submit p95 (us) | Four-source submit p95 (us) | One-source ready p95 (ms) | Four-source ready p95 (ms) |
|---|---:|---:|---:|---:|
| 1 | 80 | 153 | 41 | 63 |
| 2 | 81 | 274 | 42 | 63 |
| 3 | 76 | 216 | 42 | 63 |

All 120 samples preserve output size, source identity/ticks, observed Software
backend evidence, the eight-session cap, and zero sessions after app drop. The
unchanged runner's read-only validation statements verify all samples, source
paths/sizes, distributions, ordering, deltas, ratios, and limits; active/peak
session bounds were additionally checked. Prior checkpoint four-source ready p95
was 69-70 ms. Five-millisecond polling and host variation limit this comparison:
it is not physical input/scanout latency or proof of sustained real-time playback.

## Evidence and package

Ignored local evidence under `artifacts/phase1-multisource/`:

- `conversion-cost-baseline.log`, `conversion-cost-thread-probe-release.log`:
  unchanged serial baseline and retained-context thread-count experiment.
- `threaded-scaler-timing.log`, `threaded-scaler-lifecycle.log`,
  `threaded-scaler-workspace.log`, `threaded-scaler-clippy.log`,
  `threaded-scaler-hardware.log`, and `threaded-scaler-latency-{1,2,3}.{json,log}`.
- `threaded-scaler-package.log` and `threaded-scaler-verification.json` record
  package preparation and hashed input/code/evidence provenance.

The 27 manifest entries were rehashed successfully after the final rebuild.
Manifest SHA-256:
`A8DA59AF96519C1C0018CCEC214F224D52CF5084B6AD75C5435CD37C0CFBC562`.

The rebuilt portable executable matches the release build, SHA-256
`3A4D88D8672810349A2921C90371ED7E1A679CB0821A66BDA0953ADFF1F574C8`.
The approved launcher passed `--verify-runtime`; packaged FFmpeg/FFprobe load with
only package/Windows paths, and eleven FFmpeg runtime DLL copies match their
pinned sources. `PACKAGE-STATUS.json` remains `smoke_status: not_run`.
No GUI was launched. The prior executable/status are preserved in
`threaded-scaler-previous-package.zip`. Historical smoke reports remain unchanged.

Next: requalify sustained resource use and windowed playback with accurate threaded
conversion. The existing pre-change soak/windowed reports are historical evidence,
not qualification of this package.
