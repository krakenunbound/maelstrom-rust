# Exact rational export clocks

## Defect and correction

Export previously divided the project's integer frame-rate numerator and denominator
as `f64`, then formatted six decimal places for FFmpeg. This changed the requested
clock: `24000/1001` became `23.976024`, which the bundled FFmpeg resolved to
`2997003/125000` with time base `125000/2997003`, rather than `1001/24000`.
The corresponding `30000/1001` expression became `29.970030`, resolving to
`2997003/100000` rather than the original fraction.

The export graph builder now retains `numerator/denominator` strings for its
background, moving videos, still images, title inputs/filters, and transition
mattes. FFmpeg reduces equivalent fractions itself. This changes no source-time
selection policy (`round=up`), duration boundary, audio filter, encoder choice,
resolution, preview path, project schema, or dependency. The graph is still built
off the UI thread. This is a clock-precision fix, not a playback speed claim.

## Regression proof

Two new tests in `crates/nle-export/src/lib.rs` fail against the old decimal
implementation and pass after the correction:

- `export_graph_keeps_exact_frame_rate_for_every_visual_source` builds a snapshot
  containing two videos, a still image, a Japanese title, and a dip-to-black
  transition. It verifies every clock-bearing argument/filter for six project
  ratios, including an unreduced ratio and equal `u32::MAX` operands.
- `real_ffmpeg_export_cadence_retains_exact_rational_time_base` takes the actual
  generated background and video cadence expressions, runs them through bundled
  FFmpeg, and checks `showinfo`'s rational time base and frame rate. Seven cases
  cover `24000/1001`, `30000/1001`, `60000/1001`, `25/1`, `24000/1007`,
  `48000/2002` (reduced to `24000/1001`), and equal `u32::MAX` operands (1/1).
  Each invocation reuses the ten-second kill-and-wait child wrapper; diagnostic
  files are scoped to the test and removed on drop.

The live test is optional when `FFMPEG_DIR` is unset. The Phase 0 runner now calls
it explicitly with the pinned runtime, so normal scenario qualification cannot
silently skip this check. Existing shifted ProRes/DNxHR VFR tests still pass all
20 cases and 88 frame identities/counts/timestamps; see
[the source-identity contract](shifted-vfr-export-parity.md).

## Verification checkpoint — 2026-08-31

Source base: `9bc55bcca5872f726d0113ab7b6d340e49a91271`, plus the recorded diff.
Commands ran from the workspace using the full Cargo path and the project runtime
runner, with `FFMPEG_DIR` pointing to `.deps/ffmpeg-project-8.1` and both generated
ProRes/DNxHR fixture environment variables set for the release workspace run.

- Focused before proof: both new tests fail on decimal clock expressions.
- Export crate: 57 release tests pass, including all seven live clock cases.
- `cargo test --workspace --release -- --test-threads=1`: 777 pass, 24 ignored.
- `cargo clippy --workspace --all-targets --release -- -D warnings`: pass.
- `cargo fmt --all -- --check` and `git diff --check`: pass.
- `scripts/Test-MediaFixtures.ps1`: seven existing fixture contracts pass.
- Updated `scripts/Run-Phase0Scenarios.ps1`: focused codec/VFR/export-clock tests
  and seven scenarios pass. Existing fixtures were validated separately, then
  reused with `-SkipFixtureValidation` to avoid regenerating them.
- Parent diff review complete. Independent review unavailable (agent limit);
  no independent review pass is claimed.

Local logs and a source/evidence hash snapshot use the `exact-export-rate-` prefix
inside ignored `artifacts/phase1-multisource/`. The scenario report is
`artifacts/phase0-scenarios/exact-export-rate-scenarios.json`.

The live clock test uses a null sink, not every production encoder/container.
It does not establish multi-hour audio/video drift, H.264 hardware parity, color
parity, full playback performance, or completion of the broader Phase 1 gate.

At the source-test checkpoint, the portable executable was still
`5DD49EF46A5BEBD6226B17AD2A8CE0A4E1749C1738681017DA5369B3C21F37B2` and did
not contain the correction. The following build replaces it without a GUI launch.

## Portable package checkpoint — 2026-08-31

The package was rebuilt from clean source commit
`ca99dfddc61e070186fab09f634b807bc76feb7b` using the existing packaging script's
`-SkipSmoke` path. All 23 files in the previous package were archived and each
ZIP entry verified by length and SHA-256 before replacing the exact non-reparse
`dist/Maelstrom-Windows-x64` directory. The previous package is recoverable from
`artifacts/phase1-multisource/package-exact-rate-ca99dfd/previous-package.zip`
(SHA-256 `8F68D955BC8D84852A768EA362DBE7BA676B7699F9965B9D2823A1C36BE296FD`).

The rebuilt executable SHA-256 is
`41BD27272A4CFDE7843E8941FACB04D18B6C09AF7FDC9589EA551CF6ED8C1F7F`,
matching both `target/release/nle-app.exe` and `PACKAGE-STATUS.json`.
Packaging completed at `2026-08-31T19:57:10.2684816Z`.

- The 23-file before/after inventory differs only in `Maelstrom.exe` and
  `PACKAGE-STATUS.json`. Models, license notices, and runtime files are unchanged.
- All 13 pinned FFmpeg tools/shared-library hashes match `BUILD-SHA256SUMS.txt`.
  `vcruntime140.dll` matches the explicitly selected, authorized Visual Studio
  VC Redist source; no individual DLL was downloaded or installed.
- All 15 package executables/DLLs are AMD64 PE files. Static imports resolve to
  adjacent files, known Windows modules present on this host, or recognized
  Windows API-set contract names. API-set runtime resolution was not tested.
- Packaged FFmpeg and FFprobe run their version checks successfully. The exact
  `H:\Maelstrom Rust\Launch-Maelstrom-Editor.bat --verify-runtime` check succeeds;
  this branch exits before launching the editor.
- Package verification was performed while tracked source was still clean at
  the build commit. No Cargo/compiler/editor/media-tool processes remained.

Build and verification helpers/logs are retained under ignored
`artifacts/phase1-multisource/`; the complete file/import/hash inventory is in
`package-exact-rate-ca99dfd/verification.json`. No package or binary was pushed.

The package now contains the correction, but its `smoke_status` deliberately
remains `not_run`. These are local static/runtime-tool checks, not clean-host,
dynamic GPU-library, GUI playback/export, or windowed performance qualification.
Those checks remain open and the editor has not been launched.
