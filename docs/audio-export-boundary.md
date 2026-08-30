# Finite audio export boundaries

The 2026-08-30 source-tree verification exposed an intermittent two-second
equal-power audio-crossfade export runaway. FFmpeg remained CPU active,
produced about 12.8 MB, and reported AAC timestamps near the negative i64
limit. Its exact child was stopped after the export suite had run for
173.34 seconds. A preceding run and a later serial run passed; that did not
close the failure.

## Reproduction and mechanism

Replaying the saved three inputs, filter graph, and command directly through
the approved FFmpeg bundle reproduced six timeouts in twenty runs. Each
replay had a three-second watchdog that killed and reaped only its own child.
The other fourteen completed successfully. This reproduction does not run
the editor, Rust export worker, or shared-clip history code.

The final audio chain was `amix=inputs=2:normalize=0,apad,atrim=duration=2`.
Unlimited padding relied on valid timestamps reaching the duration trim.
The first retained AAC warning followed DTS 95232 and jumped near i64::MIN;
subsequent warning output grew continuously. This establishes a failure of
the timestamp-dependent termination boundary. It does **not** identify the
exact upstream FFmpeg scheduling/filter defect that first supplied the bad
timestamp.

[FFmpeg's filter documentation](https://ffmpeg.org/ffmpeg-filters.html#apad)
specifies that unconfigured `apad` produces unlimited silence. Its
[`atrim` documentation](https://ffmpeg.org/ffmpeg-filters.html#atrim) distinguishes
timestamp limits from sample counters. The
[`asetpts` examples](https://ffmpeg.org/ffmpeg-filters.html#setpts_002c-asetpts)
describe constructing an audio clock from the emitted sample count.

## Change and preserved behavior

The final 48 kHz mix now uses:

```text
apad=whole_len=S,atrim=end_sample=S,asetpts=N/SR/TB
```

`S = ceil(project_duration_ticks * 48000 / PROJECT_TIMEBASE)`, calculated in
i128 to avoid overflow. The final partial sample covers less than 1/48000
second. Both mixed audio and generated silence use this boundary. The
counter limits output independently of timestamps; the final clock follows
the samples already placed on the timeline. Clip source offsets, integer
delays, crossfade envelopes, channel gains, effects, and mixer normalization
are unchanged. No runtime DLL or project schema changed.

The export worker also drains stderr into a 64 KiB tail instead of an
unbounded string. A malformed stream cannot consume unbounded application
memory through repeated error messages. Ordinary terminal errors still show
the last four lines. An OS error while polling a child now kills/reaps that
exact child and joins its pipe readers before propagating the error.
Cancellation retains the existing behavior; there is no arbitrary production
export-duration timeout.

## Verification

- Saved-command baseline: 6/20 timeouts, with 487,956 timestamp warnings across
  the terminated runs. All failures are retained.
- Finite padding, sample trim, and generated clock: 20/20 complete, no timestamp
  warnings. All twenty MP4 files are byte-identical to a successful baseline
  output: SHA-256 `A62D7F4052146BEE38F5B37E8D489B0D62229CAAFBEFCA0000F5CC440D796181`.
- Diagnostic variants: finite padding/trim without clock repair passed 20/20;
  clock repair before the original padding passed 10/10. Adding timestamp
  logging to the original graph passed 10/10, illustrating its timing-sensitive
  nature rather than proving the original chain safe.
- Production crossfade test: twenty independent release Cargo invocations pass
  the existing before/midpoint/after RMS assertions with the corrected graph.
- Two deterministic real-media regressions fail against the original boundary
  and pass against the fix. Two delayed tones pass through `amix`; explicit
  missing and negative clocks are observed before the boundary. Tests require
  exactly 48,000 PCM samples, the authored tone/silence regions, and a zero-based
  continuous sample clock. An empty upstream stream must also yield finite
  valid silence. The old graph emits 265,664 samples in the tone test and has
  invalid output PTS in the empty-stream test.
- Test children have a ten-second watchdog, RAII kill/reap cleanup, and a 1 MiB
  output cap. The cap is only a failure guard: exact expected samples must still
  pass. Additional tests cover fractional/maximum duration arithmetic and
  draining a large generated error stream while retaining its bounded tail.
- Full parallel release workspace verification passed 720 tests in each of
  three consecutive runs. Strict all-target workspace Clippy passed, and
  independent review found no blocking issues. No editor was launched; all
  task-owned test and FFmpeg processes were cleaned up.

Run focused checks from the workspace, with `FFMPEG_DIR` pointing to
`H:\Maelstrom Rust\.deps\ffmpeg-project-8.1`:

```powershell
& 'C:\Users\The Kraken\.cargo\bin\cargo.exe' test -p nle-export --release --lib audio_boundary_tests -- --nocapture
& 'C:\Users\The Kraken\.cargo\bin\cargo.exe' test -p nle-export --release --lib tests::real_ffmpeg_equal_power_audio_crossfade_keeps_midpoint_energy -- --exact --nocapture
```

Ignored evidence is retained in `artifacts/phase1-multisource`:
`shared-clip-export-stall/` contains original inputs/command, the bounded replay
script, all replay outputs/logs, and per-variant summaries. Focused logs are
`audio-export-old-boundary-regression.log`, `audio-export-boundary-final.log`,
`export-error-tail-tests.log`, and `audio-export-crossfade-{1..20}.log`.
Broad checks are `audio-export-workspace.log`, `audio-export-workspace-{2,3}.log`,
and `audio-export-clippy.log`; `audio-export-summary.json` indexes measured results
and log hashes.
Summary SHA-256: `173D1FC9AF82421CA97AEA4A5EBDB79E29D747AA3E0FE5B415B5C2AB9B9E628D`.
These local CPU/FFmpeg checks do not qualify live UI, cross-hardware playback,
or packaged ten-minute soak behavior.
