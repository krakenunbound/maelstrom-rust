# Windows monitor worker wake scheduling

## Cause and controlled evidence

The corrected restricted-CPU audio workload exposed 14–16 ms four-layer
submission p95 despite bounded command queues. Earlier stage probes located
almost all of that delay around `SourceLaneActor::submit`'s wake notification;
see `docs/restricted-live-audio-submission.md` for preserved failures.

A further test-only probe on source `628742c3b41ceb18e533fd6c7724b329c76d6f19`
measured both notification wall time and the calling thread's CPU cycles. Of
2,344 notifications, 112 took at least 8 ms (median 10,701 us) but used a median
22,900 CPU cycles. Calls below 1 ms used a median 13,329 cycles. All queries
succeeded and no records overflowed. This rules against large CPU computation
as the main cost; cycles are **not** converted into elapsed time, as required by
[Microsoft's API documentation](https://learn.microsoft.com/en-us/windows/win32/api/realtimeapiset/nf-realtimeapiset-querythreadcycletime).

The same instrumented executable then ran an on/off/on control sequence. All
runs used four independent Full-1080p sources, the same stereo AAC audio, four
allowed CPUs, and the original 30-second gate and thresholds:

| Automatic worker wake boosts | Submission p95 | Gate result | Audio underrun frames |
|---|---:|---|---:|
| Disabled in test actors | 89 us | Passed | 0 |
| Windows default restored (fresh process) | 14,262 us | **Failed** | 0 |
| Disabled again in test actors | 59 us | Passed | 0 |

Each disabled run queried all four live actors: disabling boosts succeeded and
their base priority remained `THREAD_PRIORITY_NORMAL`. No process, UI, audio,
FFmpeg codec-thread, resolution, filter, or capacity setting was changed. The
first disabled run reduced per-notification p95 to 10 us, with no notification
over 1 ms. All four sources remained active throughout all three runs.

The repeatable intervention supports automatic wake-priority boosting as the
dominant submission delay on this restricted host. Windows documents boosting
threads when wait conditions become satisfied; the additional priority lets a
woken compute worker preempt the caller still submitting the other layers.
See [Windows priority boosts](https://learn.microsoft.com/en-us/windows/win32/procthread/priority-boosts).
These probes do not constitute an OS context-switch trace or cross-hardware proof.

## Production correction

`nle-decode` configures both standalone monitor schedulers and coordinated source
actors once, on their own thread before FFmpeg initialization. On Windows it
calls `SetThreadPriorityBoost(GetCurrentThread(), TRUE)` to suppress automatic
boosts. It does not lower the worker's base priority or raise another thread's
priority. Non-Windows platforms retain their existing behavior.

No locks or allocation are added to per-frame submission. The wake channel,
latest-wins routing, session/cache caps, image quality, and audio implementation
are unchanged. If Windows rejects the configuration, decoding continues with
Windows defaults and the first failure is logged once per process. That failure
branch has been inspected, not forced in runtime tests.

This is compute-thread scheduling, not background resource mode: disk and memory
priorities are not changed. Disabling boosts may affect behavior under competing
loads, so resource, latency, audio, and windowed/cross-hardware qualification must
remain explicit. It does not fix or excuse the previously observed audio lock
failure, whose precise owner remains unattributed.

## Verification and evidence

The actual-thread checks in both existing prewarm tests fail before the startup
calls are added: boost disabled is false, base priority is still normal. They
pass afterward, while retaining their cache/session/notification assertions.
A new isolated-thread test proves idempotence, preservation of a deliberately
non-default base priority, and no change to caller boost policy or process class.
The release workspace passes 773 tests (24 opt-in tests ignored), strict
all-target Clippy, and formatting. The parent reviewed the final implementation;
a separate reviewer could not be dispatched because the agent thread limit was
reached, so no independent-review pass is claimed.

An initial failing assertion was placed under a test ownership lock and caused
poisoned-lock teardown to abort. The test now gathers policy results under the
locks and asserts only after releasing them. Both the initial abort and the clean
before-fix assertion failures are preserved as `worker-policy-before.log` and
`worker-policy-before-clean.log`; they do not indicate a product memory overwrite.

All probes and test-only scheduling switches were removed before validation.
Ignored local evidence is under `artifacts/phase1-multisource/`, with prefixes
`wake-cycles`, `wake-no-boost`, `wake-control`, and `wake-no-boost-repeat` followed
by `-4-live-audio`. Reports, live affinity, raw cycles, source patches, archived
executables, and arithmetic/hash verification snapshots are retained. The latter
three runs use the same archived `wake-no-boost-test-binary.zip`. These are local
post-run snapshots, not signed execution attestations.

## Uninstrumented production qualification

Clean source `2a7c72d9eb4a40b077ce39afbe24eb2f490f8606` passes both 30-second
native-audio gates. No test-only probe or scheduling override is present:

| Allowed CPUs | Duration | Four-layer submissions / layer requests | Submit p95 / max | Clock drift / max stall |
|---|---:|---:|---:|---:|
| 4 | 30.001509 s | 586 / 2,344 | 55 / 465 us | 1,509 us / 30 ms |
| 8 | 30.001842 s | 586 / 2,344 | 51 / 322 us | 1,842 us / 23 ms |

Both preserve all four Full-resolution sources, pass the deliberate slow-source
isolation/recovery checks, and have zero observed audio underruns, callback-lock
failures, late discards, or monitor errors. Cache peaks are 1,069,977,600 bytes
under 1 GiB; four peak sessions remain below eight, with zero after App drop.
Each test process has 26 actual affinity observations (`F` / `FF`). These passes
do not prove the earlier intermittent audio-lock failure can never recur.

The four-CPU ten-minute **resource** gate also passes at that clean source:
600.034830 seconds, 14,116 cycles, and 56,464 requested/completed/presented/uploaded
frames. Submission p50/p95/max is 55/74/2,066 us; frame-ready p50/p95/max is
45/60/146 ms. Zero frames were dropped and zero decode errors occurred. There
were still 10,843 hold observations and 10,847 late-frame observations; this is
not a zero-latency playback claim. Cache peak is 215,654,400 bytes under 1 GiB;
five sessions/actors remain below eight, zero sessions survive App drop, and
working-set growth is 31,223,808 bytes under the 1.5 GiB diagnostic allowance.
The wrapper captured 517 live memory/affinity samples, all at mask `F`.

Frame-ready p95 of 60 ms is higher than the earlier `b28cb4e` four-CPU resource
run's 50 ms, and cycle count is lower (14,116 versus 18,132). Those separate runs
are not a controlled timing comparison. Do not claim faster end-to-end scrubbing;
perform a paired frame-readiness comparison before drawing that conclusion.

Reports/audits use `worker-policy-{4,8}-live-audio*` under the multisource artifact
directory and `worker-policy-4-600s-1-*` under `artifacts/phase1-sustained/`.
The raw-percentile, fixture/runtime, actual-affinity, bounds and cleanup audits
pass. All three runs use test-executable SHA-256
`D78291F2C48C4C823C15DCF5B727E1689B0399B77BA65CC375D67B6AE0E98D40`,
archived as `worker-policy-audio-test-binary.zip` in the multisource directory.
The resource soak contains no native audio; audio evidence is from the separate
30-second runs above. Neither test launches the GUI.

The eight-CPU ten-minute resource gate now also passes on clean source
`c12e0cac7a404aa9aa88a3ea8adfe0614da4ef48` (documentation-only changes since
`2a7c72d`; the same uninstrumented executable hash above). Its 600.044041 seconds
cover 18,149 cycles and 72,596 matching requested/completed/presented/uploaded
frames. Submission p50/p95/max is 44/66/614 us; frame-ready p50/p95/max is
34/46/83 ms. There are zero drops/errors, but 11,940 hold observations and 11,944
late-frame observations. Cache peak is 215,654,400 bytes, five sessions/actors
remain below eight, and zero sessions survive App drop. Working-set growth is
29,794,304 bytes; all 522 live affinity samples have mask `FF`. Artifacts use
`worker-policy-8-600s-1-*`, including the passing arithmetic/hash/cleanup audit.

## Paired frame-readiness follow-up

Eight serialized 30-second diagnostic runs compare automatic wake boosts
(`legacy`) with the production disabled-boost policy (`base`). Each CPU budget
uses order **legacy, base, base, legacy**, forming two opposite-order pairs. All
use the same executable, original four Full-1080p sources, seek pattern, resource
caps, scaler policy and 1,000 us submission limit. These short silent resource
runs are not sustained qualification, native-audio tests, or windowed playback.

Temporary `test-hooks` code changes only the startup boost setting and records
actual thread ID, boost state, and base priority. Every run confirms five unique
workers (four foreground plus one prewarm actor), the requested boost state,
and unchanged normal base priority. No probe runs in the per-frame submission
path. Source hashes, runtime/fixture hashes and actual `F`/`FF` affinity are
checked. The original report schema, workload and assertions are unchanged;
worker evidence is a separate sidecar.

| CPUs | Order / setting | Cycles | Submit p95 | Frame-ready p50 / p95 | Submission gate |
|---|---|---:|---:|---:|---|
| 4 | 1 / legacy | 666 | 1,099 us | 48 / 63 ms | **Failed** |
| 4 | 2 / base | 695 | 70 us | 46 / 61 ms | Passed |
| 4 | 3 / base | 669 | 64 us | 47 / 63 ms | Passed |
| 4 | 4 / legacy | 663 | 1,157 us | 48 / 62 ms | **Failed** |
| 8 | 1 / legacy | 1,090 | 222 us | 28 / 43 ms | Passed |
| 8 | 2 / base | 931 | 69 us | 34 / 45 ms | Passed |
| 8 | 3 / base | 911 | 69 us | 34 / 45 ms | Passed |
| 8 | 4 / legacy | 884 | 239 us | 35 / 47 ms | Passed |

All eight reports pass their provenance/arithmetic/resource audits, have zero
drops/errors, and retain the original gate status (including both four-CPU
control failures). Fixed-policy submission gains repeat at both CPU budgets.
Frame-ready p95 differences change direction: fixed minus legacy is -2/+1 ms
at four CPUs and +2/-2 ms at eight. The first eight-CPU pair loses about 14.6%
of cycles/second with the fixed policy; the reverse-order pair gains about
3.1%. This is **not** evidence of a consistent readiness regression, but neither
does it establish non-inferiority or faster end-to-end scrubbing. The sample
is small, system load is not controlled, and per-cycle samples are not
independent experiment repetitions. Do not pool them into a spurious confidence
claim or use this comparison to declare the interactive latency requirement met.

Local evidence is `artifacts/phase1-sustained/paired-worker-policy-*`, including
the diagnostic source patch against `c12e0ca`, per-run reports/worker sidecars,
affinity, logs, verification snapshots and the joined comparison JSON. Test
executable SHA-256 is
`09642B82E0D80CC14304FA74AF3B1E4C2EF834EA63F5E2022F09A890B991DCFB`;
`paired-worker-policy-test-binary.zip` SHA-256 is
`C5E31CE3D6FB475BECB040800DCBC7742A1C6F0166CD4BAD69F1F54CEFCBD1DE`.
The archived executable and every per-run evidence hash were checked before
removing instrumentation. These remain local unsigned evidence snapshots.

All temporary source hooks and environment switches are removed; the three
affected source files match the clean production commit. Fresh release workspace
tests (773 passed, 24 ignored), strict all-target Clippy and formatting pass.
No new production scheduling change is justified by these mixed readiness
results. Independent review and windowed/cross-hardware behavior remain open.
The package was unchanged at the end of this comparison; subsequent assembly
is recorded below. No editor was launched.

## Portable package checkpoint

The portable Windows package was rebuilt from clean source
`c60335829ba3d8c25d22ecd6ee8cc7b1a9750b30` using the reviewed
`scripts/package-windows.ps1 -SkipSmoke` path. It now includes silent speculative
prewarming and the Windows monitor-worker wake policy, in addition to the
existing CPU-count/conversion fixes. The packaged executable matches the release
build at SHA-256
`5DD49EF46A5BEBD6226B17AD2A8CE0A4E1749C1738681017DA5369B3C21F37B2`.

All 23 files in the prior package were archived and checked byte-for-byte before
the packaging script replaced its validated, non-reparse output directory.
Recovery archive:
`artifacts/package-worker-policy-c603358/previous-package.zip`, SHA-256
`3D0BAB2361CD791CB06B72183285154098FD9B12EFA46841678DF98E0AFF0BDC`.
Only `Maelstrom.exe` and `PACKAGE-STATUS.json` differ between the old and new
23-file packages. Models, licenses, runtime libraries and other support files
are unchanged. No system DLL was installed, downloaded, or replaced.

The package passes these local, non-GUI checks:

- All 13 bundled FFmpeg command/runtime files match the pinned build checksum
  inventory; the additional app-local VC runtime matches the authorized installed
  Visual Studio CRT. The frozen LGPL FFmpeg 8.1 line is unchanged.
- All 15 PE files are AMD64. Static imports resolve to adjacent packaged files,
  known Windows modules present on this host, or explicitly classified Windows
  API-set contracts. Contract resolution and dynamically loaded GPU libraries
  are not proved by this static inspection. API-set names are virtual aliases,
  not necessarily files on disk; see [Microsoft's API-set documentation](https://learn.microsoft.com/en-us/windows/win32/apiindex/windows-apisets).
- Packaged FFmpeg and FFprobe run their version checks successfully. The exact
  full-path launcher passes `--verify-runtime`, whose inspected branch returns
  without launching the editor. All 12 required adjacent DLLs are present.
- The empty packaged model registry matches the source; optional restricted
  runtime/model locations and the launcher remain untouched.

`artifacts/package-worker-policy-c603358/verification.json` records complete
file hashes, the backup, static imports, architecture, loader checks and scope.
This is an unsigned local snapshot, not a clean-machine installation test.
`PACKAGE-STATUS.json` deliberately remains `smoke_status: not_run`; historical
smoke reports do not qualify this new executable. Windowed playback validation
still requires explicit approval to use
`H:\Maelstrom Rust\Launch-Maelstrom-Editor.bat`. No GUI launch occurred.
