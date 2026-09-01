# Shifted VFR export source identity

The existing export graph passes 20 new local source-time cases for the generated
ProRes Standard and DNxHR HQX MOV fixtures. No production defect was found in these
cases, and no production timing, decoder, quality, or runtime setting was changed.
This checkpoint adds regression coverage and integrates it into the existing
Phase 0 scenario runner; it does not claim complete export or color parity.

## Shifted/reordered MPEG-4 follow-up — 2026-08-31

The generated `vfr-reordered-shifted-mpeg4.mp4` fixture extends the same production-graph gate
with a three-second origin, yuv420p MPEG-4 Advanced Simple Profile, irregular local timestamps
`0, 33333, 100000, 133333, 200000, 266667, 366667, 400000`, and I/B/B/P/B/B/P/P packet reordering.
It adds ten head/trim/slip/tail/final-frame cases across 30/1 and 30000/1001 project rates. All 44
exported MP4-case frames match the source identity selected by preview, and output timestamps retain
the rational project grid. Combined with the two MOV fixtures, the gate now covers 30 cases and 132
exported frames. No production graph change was required.

This closes one preview/export timing gap; it does not prove export color fidelity, hardware
encoding, long-GOP camera media, or cross-machine performance.

Subsequent production correction: [exact rational export clocks](exact-export-frame-rate.md)
removes six-decimal frame-rate rounding. These source-identity checks still pass;
the original checkpoint below describes its own test-only changes.

## Contract and evidence

Both 320x180 fixtures contain eight 10-bit 4:2:2 frames with a 7-second container
origin. Independent FFprobe timestamps normalize to local microseconds:
`0, 41667, 125000, 166667, 250000, 333333, 458333, 500000`.

The new `crates/nle-export/src/vfr_export_tests.rs` tests exercise the existing
`EditorState`, frame-time index, immutable export snapshot, `ExportPlan`, and
production filter-graph builder. Expected source identities are calculated by
flooring each logical source tick against the independently probed timestamps.
The editor's logical `source_tick` must keep advancing while its `decode_tick`
holds the correct indexed frame. A real slip operation changes source content
without moving the clip. Already-trimmed ranges are inserted directly.

Each codec runs all five cases at both 30/1 and 30000/1001 project rates:

| Case | Source start | Exported project frames | Expected source-frame indices |
|---|---:|---:|---|
| Head | 0 us | 6 | 0, 0, 1, 1, 2, 3 |
| Trimmed range | 100,000 us | 6 | 1, 2, 3, 3, 3, 4 |
| Slipped range | 100,000 + 45,000 us | 6 | 2, 3, 3, 3, 4, 4 |
| Tail range | 400,000 us | 3 | 5, 5, 6 |
| Final source frame | 500,000 us | 1 | 7 |

At 30 fps the tail case ends exactly at the exclusive 500,000 us source boundary;
the frame at that boundary must not leak into the output. Durations use whole
project frames, so these tests do not define a new partial-final-frame policy.
In total, 88 exported frames have the expected identities and frame counts.
Output PTS starts at zero and follows the rational project-frame grid within
one microsecond of FFprobe's decimal rounding. Identity itself has no tolerance.

The test decodes each entire source sequentially with a separate FFmpeg CLI call,
then identifies output frames by the uniquely closest full-frame RGB reference.
It retains the production export graph but substitutes the built-in MPEG-4 encoder,
following the existing five-color VFR test. Consequently this proves source-frame
selection through the graph, not bit-exact pixels, ten-bit output preservation,
H.264 encoder conformance, hardware export, HDR, or broad color-management parity.

## Regression sensitivity and verification

Temporarily changing only the graph's `fps` rounding policy from `round=up` to
`round=near` makes both tests fail on the head case: output identities become
`0, 1, 1, 1, 2, 3` instead of `0, 0, 1, 1, 2, 3`. The mutation is removed, and
both tests pass with the unchanged production policy. This demonstrates that the
new checks reject premature selection of a future VFR frame.

The new explicit FFmpeg/FFprobe command helpers reuse the existing ten-second
bounded child wrapper, which kills and waits on its child on failure. Existing
export-plan probing is unchanged. Temporary output,
filter, raw-frame and diagnostic files are scoped to the test and cleaned on drop.
The new tests share the real-media test mutex with existing export/audio tests.

Fresh verification passes 775 release workspace tests (24 opt-in tests ignored),
strict all-target release Clippy, formatting, all seven fixture contracts, and
the updated Phase 0 runner including its seven scenarios and these two explicit
supplied-fixture tests. The parent reviewed the final diff. A separate reviewer
could not start because the agent thread limit was exhausted; no independent
review pass is claimed.

Logs and a hash snapshot are retained under `artifacts/phase1-multisource/` with
the `shifted-vfr-export-` prefix. Fixture identities remain:

- ProRes: `523DD0AA1941B13E2810C97E44357B3469E869E3BBDB5F33C0392CF4F6D7FAEE`
- DNxHR: `6038CDE607392E49A09D80F92B75DA25A9CFF2034435202C2D4C80DCF2330C1A`

This is small-frame local Software evidence, not a playback performance result.
The packaged executable remains `5DD49EF46A5BEBD6226B17AD2A8CE0A4E1749C1738681017DA5369B3C21F37B2`.
No package rebuild was needed for test-only changes, and no editor was launched.
Broader codecs, real camera sources, cross-backend/color parity and windowed
qualification remain open.
