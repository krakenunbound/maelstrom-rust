# Scrub seek latency and delayed-frame correctness

Two clean final-package repetitions pass the local one/four-source windowed
workload. Four-source matching-frame p95 falls from 523.0007 ms to 86.1059–104.0912 ms
on Intel UHD 770 and from 455.5999 ms to 73.8769–91.8742 ms on RTX 3090.
These are local Software-decoding MPEG-4 scrub measurements, not a claim about
all codecs, continuous playback, audio continuity, or physical input/display latency.
Two additional attempts were invalidated by unexpected playhead changes and are
preserved below; they are not treated as passing timing trials.

The subsequent [input integrity follow-up](windowed-input-integrity.md) reproduces
the incoming-pointer mechanism and repairs failure evidence preservation. It does
not establish the origin of these historical interruptions.

## Cause and implementation

The previous path subtracted five seconds before every backward keyframe seek.
It also reused the open decoder for forward scrub jumps as large as five seconds.
For the five-second fixtures, this caused repeated traversal from near the beginning.
Reverse traversal converted intermediate frames to Full-1080p RGBA for the cache;
the baseline four-source runs performed 5,419 conversions on Intel and 4,324 on
NVIDIA across startup, warmups, and measured inputs. The last upload preceded
matching presentation by only a few milliseconds in most samples.

`StickyMonitor::decode` now tries a stream-specific backward keyframe seek at the
target. If that seek fails, yields no frame, or initially yields a frame more than
one microsecond after the target, it retries once with the existing five-second
lookbehind. The first late frame is not published. The retry flushes the decoder,
rechecks cancellation and the latest target, and preserves reverse-cache direction.
The [FFmpeg demuxing API](https://ffmpeg.org/doxygen/trunk/group__lavf__decoding.html)
defines the stream time base and keyframe seek operation; the fallback additionally
handles packet-DTS versus frame-PTS behavior verified with the reordered MPEG-TS fixture.

Nearby scrubs still reuse the decoder within 250 ms. Non-scrubbing forward reuse
remains five seconds. Output quality, session/cache caps, progressive-frame
convergence, request/generation identity, and the one-microsecond rounding rule
are unchanged. No extra decoder session or UI-thread media operation was added.

The expanded B-frame tests exposed an existing EOF edge case: an earlier frame
request can start draining the decoder, while later requests still need buffered
frames. A repeated null packet returns FFmpeg EOF; it must not prevent receiving
the remaining frames. The drain path now accepts this already-draining state and
preserves all other errors. Both generated MPEG-4 B-frames and supplied H.264
previously failed at the final 4,966,667 µs request; they now complete correctly.

## Windowed results — 2026-08-30

Each case uses eight warmups and forty measured inputs, a 1920×1080 DX12 surface,
and Full 1920×1080 requests for every source. The same four fixture hashes and
unchanged validator apply. All observed decoder backends were Software.

| Adapter | Sources | Baseline matching-frame p95 (ms) | Final run 1 / run 2 matching-frame p95 (ms) | Final input CPU p95 range (ms) | Final frame CPU p95 range (ms) |
|---|---:|---:|---:|---:|---:|
| Intel UHD 770 | 1 | 116.2329 | 61.6062 / 61.4555 | 0.3638–0.4252 | 1.0487–1.0580 |
| Intel UHD 770 | 4 | 523.0007 | 104.0912 / 86.1059 | 0.3665–0.3857 | 1.1126–1.2168 |
| RTX 3090 | 1 | 322.1114 | 61.4569 / 67.3591 | 0.3090–0.3225 | 0.8849–0.9176 |
| RTX 3090 | 4 | 455.5999 | 73.8769 / 91.8742 | 0.3727–0.4141 | 0.8934–1.0985 |

Input CPU p95 remains ≤1 ms and full-frame CPU p95 remains <8 ms. No new relative
latency threshold was invented. Cache peaks stay below the 1 GiB cap; peak sessions
remain two for one source and seven for four sources, below eight. Monitor errors
are zero in the clean runs. Four-source conversions drop to 770–852 on Intel and
792–839 on NVIDIA. These accumulated worker counters include startup/warmup and
can overlap between workers; they are not summed wall-clock latency.

The baseline is `windowed-67a91fdd-4f70-4dc0-bfdd-607716a8f0b6` under
`artifacts/phase1-multisource/`, with its executable preserved as `baseline-Maelstrom.exe`.
Its executable SHA-256 is
`051D30A729DCAFAEC5D4E86A4FA8E80C4722FCC90CD38199924C4321D80C01F1`.
Final runs are `windowed-70128ce5-d307-46db-892b-e25179687593` and
`windowed-6610d9a7-ad39-4c95-bb13-54eee51cf36c`. The final 36,126,720-byte package
matches the release build, SHA-256
`69F330ADC16BA96990B83CEE60D851AF41C88C82C5FB5AB1DCD36E4651039AAF`.
General package smoke status remains `not_run`; this dedicated qualification does
not substitute for the general smoke suite.

The initial seek-only candidate also passed three full repetitions
(`4a143f34`, `c8886984`, `c941bbb5` run-directory prefixes), before the additional
EOF repair. They are historical corroboration, not final-package results.

### Invalidated repetitions

Final-package attempts `windowed-5304fe46-9501-4ce5-a999-8ee72b959b19` and
`windowed-91bd3123-f207-4681-8a93-630a916ab893` failed the existing workload-integrity
guard: the playhead changed outside the scripted sequence while waiting for a frame.
The first ended during Intel one-source sampling at 738,255 µs instead of the
recorded 2,220,000 µs target. The second completed its Intel cases but stopped at
the first NVIDIA one-source sample, at 310,403 µs instead of 428,000 µs.
The input change's origin is unproven; physical-pointer interference is a hypothesis,
not an established cause. Reports remain intact, no guard was disabled, and no
further retry was used to conceal these failures. All instances/owned children closed.

## Regression and evidence checks

- Original seek code fails the generated GOP-12 MPEG-4 regression with 107 packets
  for a fresh 3.5-second target; the optimized path passes the ≤24-packet bound.
  Every checked frame exactly matches RGBA from sequential decoding, including
  forward, reverse, and rational-boundary targets.
- Generated MPEG-4 B-frames exercise all 150 sequential frames and random seeks.
  Restoring only the old EOF handling reproduces the final-frame failure; restoring
  the fix passes (`scrub-seek-portable-eof-{before,after}.log`). Fixtures clean up on panic.
- Generated reordered MPEG-2 TS exercises an actual `RetryPreroll` result and exact
  sequential-reference pixels, with ten packets per tested target.
- Supplied H.264 open-GOP/B-frame media matches all sequential reference pixels and
  scrub targets using Software decoding. The approved bundle has no software H.264
  encoder; the local fixture was generated with `h264_qsv -g 12 -bf 2 -idr_interval 999`
  from five seconds of 320×180 30 fps `testsrc2`, without adding dependencies.
  FFprobe confirms 150 frames (13 I, 50 P, 87 B) and two-frame reordering;
  `trace_headers` confirms one IDR NAL in the clip. Long traversal remains necessary for this fixture's
  sparse random-access points; no H.264 latency improvement is claimed.

The local H.264 input is additionally pinned by the opt-in real-media fixture contract in
`fixtures/media/manifest.json`. `Test-MediaFixtures.ps1 -IncludeRealCorpus` requires its exact
filename, hash, stream, duration, frame/keyframe, and I/P/B evidence below
`MAELSTROM_REAL_MEDIA_ROOT`; it pins the input for the separately run Software scrub test rather
than executing that test itself, and it neither creates media nor enables a hardware or package claim.
- `cargo test --workspace --release` passes 735 tests (16 opt-in ignored), with
  `MAELSTROM_SCRUB_H264_TEST_MEDIA` and `MAELSTROM_REORDERED_VFR_TEST_MEDIA` supplied.
  Strict all-target release Clippy, formatting, and independent reviews pass.

Tests use the approved Cargo runtime runner. Logs, source fixtures, failed reports,
and raw samples live under `artifacts/phase1-multisource/`. `Verify-ScrubSeek.ps1`
independently rechecks configuration/report/fixture/executable hashes, every clean
case through the production validator, stage-counter sanity, test totals, and
absence of editor/compiler/media-tool processes. Its output
`scrub-seek-verification.json` has SHA-256
`3C770CA256976C78A31BA8BFB465B76DE66D1E2A81E328D5A2A52C7D6C118C7A`.

The remaining work includes attributing any future input-integrity interruption
using the new diagnostics, broader codec/backend/reference-machine qualification,
sustained playback/audio evidence, and reducing remaining fresh-frame latency
without weakening correctness.
