# Restricted-CPU live-audio submission investigation

Follow-up: `docs/monitor-worker-scheduling.md` records CPU-cycle evidence, the
controlled wake-boost experiment, the production correction, and passing
uninstrumented four/eight-CPU audio plus four-CPU sustained resource gates.
The historical failures below are preserved, not replaced by those passes.

## Corrected workload and observed failure

The clean source `c0ebbf385071778c5fd0fc40e568556b28e6ebcb` extends all four
video timeline clips through measurement plus warmup and asserts four exact
sources at Full resolution before every measured submission. On 2026-08-31,
the corrected four-CPU-affinity, headless Software run **failed**:

| Measurement | Result |
|---|---|
| Measured duration | 30.001963 seconds |
| Four-layer submission calls / actual layer requests | 585 / 2,340 |
| Submission p50 / p95 / max | 2,679 / 16,148 / 20,808 us |
| Unchanged submission p95 limit | 1,000 us |
| Presentations per source | 582 / 583 / 585 / 569 (minimum 120 each) |
| Monitor drops / errors | 0 / 0 |
| Audio callback-lock failures / underrun device frames | 1 / 480 |
| Audio late discards / transport loss | 0 / false |
| Device-clock drift / maximum observed stall | 1,963 us / 42 ms |
| Cache peak / cap | 1,069,977,600 / 1,073,741,824 bytes |
| Peak sessions / cap / sessions after App drop | 4 / 8 / 0 |

The deliberate top-source hold lasted 750 ms. During it, other sources presented
44 frames and the audio clock advanced 750,000 ticks; the delayed source recovered.
Unlike the older harness, this run submitted all four video layers throughout:
2,340 equals 585 times four, and the final source clock remained inside the video
timeline. The performance and audio failures are real, not empty-work artifacts.

The actual Cargo test process was observed 27 times with affinity mask `F`.
This is restricted affinity on this host, not an actual four-core machine or a
windowed playback test. No lower resolution, alternate filters, or relaxed limits
were used. The eight-CPU audio qualification was not run after this failure.

## Temporary attribution traces

Two separate 30-second runs used bounded, thread-local, test-only stage probes.
They are diagnostic workloads, not substitutes for uninstrumented qualification.
The test source also adds one second of timeline slack after the ten-second
warmup allowance, addressing the review finding that a final poll can overshoot
the measurement boundary. The media timestamp wrapping and 30-second load stay
unchanged. No thread or process priority was changed.

The first trace recorded 14,064 stage records without overflow. Four-layer
submission p95 was 15,499 us. The `SourceLaneActor::submit` interval dominated
the measured decoder-submission stages, with per-layer p95 10,100 us.

The finer trace recorded 23,440 records without overflow: ten stages for each
of 2,344 layer requests. Four-layer submission p95 was 14,300 us. Within the
actor-submit interval:

| Per-layer stage | p50 | p95 | Maximum |
|---|---:|---:|---:|
| Prune dead clients | 0 us | 0 us | 1,604 us |
| Acquire queued-client lock | 0 us | 0 us | 31 us |
| Enqueue pending client | 0 us | 0 us | 396 us |
| `wake.try_send(())` notification | 71 us | 9,018 us | 18,045 us |
| Whole actor submit (includes stages above) | 72 us | 9,034 us | 18,046 us |

The two diagnostic runs had zero observed audio underruns/lock failures. This
does not erase the uninstrumented audio failure or prove repeatable continuity.
All four video sources remained active in both traces.

These wall-time probes locate the dominant delay around the wake notification;
they do **not** distinguish channel synchronization from caller descheduling.
Windows can dynamically change scheduling priorities when threads wake, so CPU
time/context-switch evidence is needed before choosing a wake or priority fix.
See Microsoft's [thread scheduling documentation](https://learn.microsoft.com/en-us/windows/win32/procthread/scheduling-priorities)
and [thread priority guidance](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-setthreadpriority).
Do not remove notifications, add polling delay, lower image quality, raise the
performance limit, or claim a product fix based only on these traces.

Separately, the audio callback returns silence when its bounded attempts to
acquire the shared mixer lock fail. Here 480 frames match one device buffer.
The current counters cannot identify whether the producer or control thread
held that lock. Add ownership/hold-time evidence before changing that path;
an ownership marker must be set **after acquiring** the lock, not before waiting.

## Evidence and next work

Local ignored artifacts under `artifacts/phase1-multisource/` retain reports,
stdout/stderr, actual affinity samples, adapters, source patches, and archived
test executables. Prefixes are `duration-fixed-4-live-audio`,
`submit-probe-4-live-audio`, and `submit-probe-v2-4-live-audio`. Each has a
`-verification.json` produced by the independent arithmetic/hash audit. The
traces additionally have `.submit-stages.json`; their exact source changes are
`submit-probe-source.patch` and `submit-probe-v2-source.patch` against `c0ebbf3`.
The diagnostic adapters require those exact modified source-file hashes instead
of claiming a clean worktree. These are local post-run snapshots, not signed
execution attestations.

The uninstrumented test executable SHA-256 is
`E918B8ABBDD1E7E37B004F8AA7F75ED703F5CC4DED514B76AD3C67466E03BB21`,
archived in `duration-fixed-c0eb-test-binary.zip`. The diagnostic executables are
separately archived as `submit-probe-v1-test-binary.zip` and
`submit-probe-v2-test-binary.zip`. Original failed evidence is never overwritten.

Temporary stage probes were removed after archiving. The retained test change
adds the one-second coverage margin and tests the 41-second timeline boundary.
Final release validation passes 772 tests (24 opt-in tests ignored), strict
all-target Clippy, and formatting. Logs are `audio-duration-final-workspace.log`
and `audio-duration-final-clippy.log` in the same local artifact directory.
Next: distinguish wake-call CPU work/lock contention from scheduling delay, fix
the responsible mechanism with regression coverage, then repeat uninstrumented
Full-resolution four/eight-CPU audio gates and the resource/latency gates.
Callback-safe audio and windowed/cross-hardware proof remain open.

No editor was launched, runtime upgraded, or package rebuilt. The existing
package still predates silent prewarming; it is not a delivery of this work.
