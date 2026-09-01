# Phase 1 layer-toggle and backward-scrub gate

`scripts/Run-Phase1GenerationStress.ps1` is an opt-in headless integration gate
for the production monitor scheduler and generation classifier. It regenerates
and validates the four independent, dynamic 1920x1080, 30 fps, five-second
MPEG-4 sources through the existing Phase 1 multisource runner. The stress test
selects Full quality but explicitly requests 640x360 pixels to isolate scheduler
correctness. This is not a Full-1080p output or throughput qualification.

```powershell
.\scripts\Run-Phase1GenerationStress.ps1
```

The script runs the ignored Rust test through the pinned Cargo executable and
project-local FFmpeg runtime runner. It never launches the editor or a raw
test executable. The report defaults to
`artifacts/phase1-multisource/phase1-generation-stress.json`, within the existing
ignored artifact directory. `-ReportPath` may select another JSON filename in
that directory.

The gate uses 32 finite cycles of forward requests, layer disable/re-enable,
and backward requests. It checks generation and media identity, retained-frame
clearing, final request completion, bounded cache/session/actor diagnostics,
and teardown. An existing test-only worker barrier forces request supersession;
separately, captured real decoded frames are replayed after invalidation to
exercise the production stale-generation rejection path deterministically.
After re-enable, the captured event's original media occupies the same slot,
with no retained frame or proxy. The old generation must be rejected; a control
copy changing only the generation must present. This isolates generation
rejection from media mismatch or frame-convergence rejection. The captured
obsolete event is rejected again after each cycle (33 rejection checks total).
The report distinguishes replay evidence from forced request supersession; it
does not claim that a cancelled request naturally delivered an obsolete frame.

This gate preserves the scheduler's intentional progressive, converging
same-generation frame handling. Obsolete generations must never present; final
completion must belong to the latest generation and request. The final event
drain checks every accepted event's generation, so briefly accepting an obsolete
event and later recovering cannot pass. Lower-layer removal compacts monitor
slots; unaffected sources are checked by media identity after that remap.

The schema-1 report records source provenance, actual decoder backends,
operation counts, per-cycle identities, stale rejection evidence, runtime
counter deltas, 96 resource checkpoints across disable/re-enable/settle,
hard resource limits, and post-drop ownership. It is published
only after the Rust assertions pass; the wrapper independently validates the
report. A failing assertion exits nonzero and does not create a passing report.

The final 2026-08-30 local Software-backend run passed 32 cycles in 1.73 seconds:
327 requests, 303 completed frames, 305 retained-frame presentations (including
the control), 42 rejected events including the 33 intentional stale replays,
zero current decoder errors, and 96 valid resource checkpoints. The exact cache
peak was 246,988,800 bytes under its 1,073,741,824-byte cap; peak sticky sessions
were five under the cap of eight. No session, source group, live actor, or
retiring actor survived teardown. The retained report SHA-256 is
`b58f086ffde99780d76a04de984c0b5948593c4f670dd5f117d9650b3dc9954c`.

The gate was renewed on clean commit `d90283c` on 2026-08-31. It again passed all 32 cycles,
32 forward and 32 backward submissions, 33 disable/re-enable pairs, one deliberately superseded
request, and 96 resource checkpoints. The Software decoder produced 301 completed frames and 303
fallback presentations; all 39 rejected events were bounded stale/non-converging work, current
monitor errors remained zero, and eight holds plus eighteen late frames did not block final newest
request completion. Peak cache accounting was 130,867,200 bytes below the 1 GiB cap, peak sticky
sessions were five below eight, and no session, source group, live actor, or retiring actor survived
App teardown. The report is regenerated evidence, so this run does not replace the historical
immutable hash above.

App (131), decoder (71), and UI-core (210) ordinary tests passed, as did strict
all-target app Clippy. Separately, the existing release
`editor::tests::fifty_thousand_clip_editor_history_events_stay_under_two_ms`
failed its unchanged 2 ms edit/release threshold in four runs: 3.0755, 2.6410,
2.7171, and 2.6261 ms. No UI-core/timeline production code changed in this slice;
the history performance gate is reopened rather than certified by this test.
Its local logs are `generation-history-latency.log` and
`generation-history-latency-repeat-{1,2,3}.log` in the same artifact directory.

This is local software-decoder evidence. It does not establish UI latency,
native GPU presentation, integrated/discrete adapter performance, audio
continuity, or ten-minute resource stability. Those remain separate roadmap
gates.
