# Windows CPU-budget safeguard

Local Windows checkpoint, 2026-08-31. Corrects the existing monitor-scaler CPU
guard; it does not demonstrate faster scrubbing or close Phase 1 playback gates.

## Reproduction and correction

On the i7-14700K host (28 logical processors), restrict only a disposable test
launcher and its children to the first four allowed logical processors, mask
`0xF`. Rust 1.92 reports 28; pinned FFmpeg 8.1 reports four. The original monitor
scaler therefore selects two threads despite its eight-processor minimum.
`cpu-budget-four-before.log` records the actual initialized scaler option as two
and the expected-policy assertion failing. No editor or unrelated process affinity
was changed.

The Windows implementation now bounds Rust's count by FFmpeg's affinity-aware
count. Zero, negative, or unavailable counts conservatively resolve to one.
Non-Windows behavior is unchanged. The eight-processor threshold, two-thread cap,
input/output area checks, setup-failure fallback, decoder settings, accurate rounding,
selected filter, output resolution, and shared-frame ownership are unchanged.
Selection happens when a scaler context is constructed; this is not live
resizing of already-created thread pools or a global CPU reservation.

Primary implementation references:

- [Rust 1.92 Windows thread implementation](https://github.com/rust-lang/rust/blob/1.92.0/library/std/src/sys/thread/windows.rs)
  uses `GetSystemInfo` for this CPU-count query.
- [Pinned FFmpeg 8.1 CPU query](https://github.com/FFmpeg/FFmpeg/blob/9047fa1b084f76b1b4d065af2d743df1b40dfb56/libavutil/cpu.c)
  counts the process-affinity mask on Windows. No project code calls its global
  `av_cpu_force_count` override. `av_cpu_count(void)` needs no pointer arguments
  or caller-owned resources.
- [Windows process-affinity contract](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-getprocessaffinitymask).

## Verification

- The new regular unit test covers conservative counts and the eight-CPU boundary.
- The opt-in Windows test constructs the actual default Full-1080p scaler and
  reads its initialized thread option. Budgets 1/4/7 select one thread; 8/28
  select two. The test separately checks FFmpeg observes the supplied CPU budget.
- 765 release workspace tests pass; 24 opt-in tests remain ignored in that run.
  Strict release/all-target Clippy and formatting pass. Existing exact
  threaded-versus-legacy conversion tests remain enabled in the workspace gate.
- Four explicit D3D11VA/DXVA2 H.264/HEVC Main 10 tests pass under both four and
  28 allowed processors: 152 independent CLI pixel/timestamp cases per budget,
  304 total, including native 1080p and padded/small output. This is local default
  hardware-adapter evidence, not every GPU or preview/export parity.
- Independent review found no blocking issues and audited all 18 latency reports.

Focused policy proof (run through the project-local Cargo runtime runner):

```powershell
# The disposable caller must already have the intended affinity budget.
$env:MAELSTROM_EXPECT_AVAILABLE_CPUS = '4'
& 'C:\Users\The Kraken\.cargo\bin\cargo.exe' test -p nle-decode --release monitor_scaler::tests::supplied_cpu_budget_selects_bounded_scaler_threads -- --ignored --exact --nocapture
```

## Full-quality latency evidence

Baseline production source is `8bf20a25e521ec4782517f5d3becbfd2dea89e8f`, with
only the diagnostic test added before measurement. The after runs include the
Windows count fix. Both use the same four independently hashed 1080p fixtures,
normal prewarming, Software decode, and the existing paused latency test.
Each budget has three before and three after runs, each with 20 one-source and
20 four-source interleaved trials: 720 trials total.

| Allowed logical CPUs | Before four-source ready p95 | After four-source ready p95 | After four-source submit p95 |
|---|---:|---:|---:|
| 4 (`0xF`) | 106-110 ms | 108-114 ms | 153-172 us |
| 8 (`0xFF`) | 71-73 ms | 72-74 ms | 157-166 us |
| 28 (`0xFFFFFFF`) | 62-67 ms | 62-64 ms | 206-240 us |

All runs pass the unchanged 1,000 us four-source submission limit. Every sample
preserves Full 1920x1080 output, expected media IDs/source ticks, observed Software
backend, session caps, and zero sessions after App drop. The local runner reuses
the committed verifier's read-only assertions for distributions, sequence order,
source paths/sizes, deltas, ratios, and limits, with additional session-cap checks.

These results do **not** show a latency improvement. Four-CPU after results are
slightly slower at the upper end, and the five-millisecond readiness polling plus
host variation limit interpretation. The policy fix restores the intended
low-CPU safeguard without changing pixels. A few restricted logical processors
on this hybrid desktop are not equivalent to a physically lower-core machine.

The disposable launcher applies a subset of its inherited affinity before Cargo,
verifies that assignment, and restores its own mask in `finally`. The separate
policy tests verify child inheritance. Live observations also confirmed an
eight-CPU after-test process mask of `0xFF`. Individual latency JSON reports do
not embed affinity or selected-scaler counts; their association relies on the
local runner and separate policy evidence. No concurrent benchmark was launched.
The local adapter is serialized evidence tooling, not a concurrency-safe harness.

## Evidence and remaining gates

Ignored local files under `artifacts/phase1-multisource/`:

- `cpu-budget-four-before.log`, `cpu-budget-count-unit.log`, and
  `cpu-budget-{1,4,7,8,28}-policy-after.log`.
- `run-cpu-budget-latency.ps1` and
  `cpu-budget-{4,8,28}-{before,after}-{1,2,3}.{json,log}`.
- `cpu-budget-workspace.log`, `cpu-budget-clippy.log`,
  `cpu-budget-{4,28}-hardware.log`.
- `cpu-budget-package.log`, `cpu-budget-parent-audit.log`,
  `verify-cpu-budget-checkpoint.ps1`, and `cpu-budget-verification.json`.

The 99-entry post-run input/source/evidence snapshot was rehashed successfully.
It is not a signed execution attestation. Manifest SHA-256:
`75F994A59ED19501BC6054564A2854D62488AF087F7262C718FDCDF04C615930`.

The rebuilt portable executable matches the release build, SHA-256:
`03E01F2EB32BFA3B301C161C638829257C2DDD1B0A78C604E070F25D365D6DFA`.
The exact launcher passed `--verify-runtime` without opening the editor. Packaged
FFmpeg/FFprobe load with only package/Windows paths; eleven FFmpeg DLLs match the
pinned bundle and the VC runtime matches its authorized Visual Studio source.
`PACKAGE-STATUS.json` remains `smoke_status: not_run`. Historical smoke reports
are unchanged. The previous executable and status are archived in
`cpu-budget-previous-package.zip`; its executable hash was checked against the
previous `4715E175...` package. Generated package contents were rebuilt locally;
no binaries or restricted assets are added to Git.

The previous unrestricted ten-minute RGBA-packing soak is historical evidence for
its recorded source. This change still needs restricted-CPU sustained/audio
qualification, refreshed windowed playback proof, and cross-machine measurements.
Automatic goal continuation does not authorize launching the editor. No quality
reduction, new DLL, dependency, model, or runtime upgrade is part of this fix.
