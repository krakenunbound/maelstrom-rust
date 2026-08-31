# Optional proxy media

Maelstrom can generate a balanced 720p editing proxy for an imported video. Right-click the video
in the Media Pool or a video clip on the timeline, then open **Proxy Media**. The same nested menu
can cancel generation, switch between the proxy and original, retry a failure, or delete the proxy.
All entries and status badges are available in English and Japanese.

## Safety and quality contract

- Original media remains the default and is always immediately editable and playable.
- Proxy generation runs on one cancellable background worker and never blocks the UI thread.
- Only monitor video decoding may use a proxy. Audio playback and Quick Export always use the
  original source path.
- Proxy files and enable state are not serialized into `.nleproj`; reopening a project never
  depends on a derived file.
- When timeline video is analyzed after placement or project reopen, the existing analysis worker
  rediscovers a matching completed proxy in the local cache. It appears as **Proxy ready**, with
  original media still selected. Use the existing Proxy Media menu to explicitly enable it.
- Discovery adds no new worker or FFmpeg invocation. It uses the existing two-analysis-worker bound,
  never inspects files on the UI/monitor-submit thread, and does not create, prune or regenerate cache
  entries. Unused Media Pool items keep the existing deferred-analysis behavior until placement.
- Stale project replies, changed requested paths, and late replies after explicit generate/switch/
  delete actions cannot overwrite current proxy state. Missing, empty, partial, directory/symlink,
  old-profile, or oversized entries remain unavailable. Explicit enable still rechecks source
  fingerprint/file existence; decode failure still falls back to the original.
- Completion rechecks the original path, size, and modification time. A missing, stale, failed,
  incomplete, or cancelled proxy automatically leaves preview on the original.
- Original/proxy switches advance the monitor cache namespace so frames from the two files cannot
  alias under the same media ID. If another process removes a ready proxy, its decode failure
  disables the derived route and immediately retries the original.
- Temporary files are removed on failure/cancellation. A completed file becomes visible only by
  atomic rename.
- Regeneration force-replaces the deterministic artifact on the worker. Delete is also a background
  job; a locked-file failure keeps a disabled cleanup record so the user can retry safely.

## Format and storage

The first profile is video-only MPEG-4, maximum 1280×720, intra-frame (`GOP 1`) for inexpensive
seeking. FFmpeg preserves variable-frame timestamp spacing and normalizes a non-zero container start
to the source-relative origin used by monitor requests. It uses the same packaged LGPL FFmpeg as the
rest of Maelstrom and does not require the optional RTX/VSR runtime.

The opt-in real-media tests cover both the output contract and an intentionally irregular 1080p
source. They require source/proxy frame-interval parity within one millisecond after timestamp-origin
normalization.

Derived files live under `%LOCALAPPDATA%\Maelstrom\Proxy Media` (or the platform temporary folder
when local app data is unavailable). The cache is explicitly limited to 64 proxy files and 8 GiB;
oldest files are removed first. The first slice intentionally allows one generation at a time and
one fixed profile. Local cache discovery survives reopening without storing a dependency in the
project; enabled state deliberately does not persist. Portable/external proxy attachments,
generation queues, and more profiles remain later work.

## Cache discovery verification — 2026-08-31

Six new regressions cover read-only discovery, source changes/cancellation, incomplete entries,
original routing and durable snapshot preservation, explicit-action/relink protection, stale
project epochs, and a real generated-proxy save/reopen path. The real-media case runs the existing
project writer/reader and media-analysis worker, checks the same artifact is rediscovered without
regeneration (including unchanged modification time), keeps original routing by default, and then
checks explicit proxy opt-in. Saved media and audio still refer to the source. This is headless
application-state/routing evidence, not a rendered-GUI or pixel/performance qualification.

The real case requires `MAELSTROM_PHASE0_MEDIA` and the pinned `FFMPEG_DIR`; the Phase 0 runner
invokes it explicitly. Full release verification passes 783 tests (24 opt-in tests ignored), strict
all-target release Clippy, formatting, seven fixture contracts, and the updated seven-scenario
runner. The parent reviewed worker ownership, source/path checks and late-result suppression;
independent review was unavailable because the agent limit was reached.

Local logs use the `cached-proxy-` prefix in ignored `artifacts/phase1-multisource/`; the scenario
report is `artifacts/phase0-scenarios/cached-proxy-scenarios.json`. Initial test-authoring failures
(a Rust borrow and test FFmpeg-path selection) were corrected and are retained separately from
passing evidence. No production dependency, schema, codec profile, resolution default, or model
changed. Cache lookup checks identity/metadata, not full-file decode validity; the established
enable/decode fallback remains necessary for files externally corrupted after generation.

## Proxy startup validation checkpoint — 2026-08-31

`ProxyJob::start` previously read source/tool metadata and canonicalized the source on its caller,
which is the UI thread for the Generate Proxy action. These operations now run on the existing
owned proxy worker before any cache mutation. The caller receives a job immediately after worker
creation; missing/non-file inputs or tools arrive as `ProxyEvent::Failed` through the existing
notified channel. Worker-spawn failures still return synchronously. The fingerprint is captured
on the worker before encoding, and source changes during encoding still prevent publication.

Two new before/after regressions reject the previous synchronous validation: one covers four
invalid source/tool cases plus worker-thread notification; the other checks the app's generating
to failed transition without changing original-media routing or durable state. A third regression
preserves pre-cancelled request semantics. The existing source-change and cancellation tests now
wait for an actual child/progress event instead of assuming a fixed startup delay. Cancellation
notification assertions run after joining the completed worker to avoid a receive/notify race.

Verification: 786 release tests pass (24 ignored), plus both explicitly enabled real-media proxy
tests (format/origin and irregular PTS). Strict all-target release Clippy, formatting, seven fixture
contracts and seven Phase 0 scenarios pass. Logs/hash evidence use `proxy-worker-validation-` under
`artifacts/phase1-multisource/`. Parent-reviewed; independent agent unavailable (agent limit).

This establishes the proxy job's asynchronous validation contract, not a measured UI-latency
bound or whole-workflow nonblocking guarantee. The app's tool-path resolver, completion/enable/
reconciliation filesystem checks, and synchronous cancellation/reset teardown are separate
remaining lifecycle work. No image-quality, codec, frame-rate, cache-cap or dependency change.

## Portable package checkpoint

Built from clean commit `fb5c94d589edeed5aeaa99845c535b5531776a2d` without opening the editor.
The executable SHA-256 is `CDFC00DB47444A5BF0F68D1F6A824D4B20F374EED747F5C034059AF0A53DDCCC`,
matching the release build and package-status record. All 23 files are inventoried; only the
executable/status differ from the previous package. Thirteen pinned runtime hashes, the authorized
VC runtime copy, fifteen AMD64/static-import inventories, FFmpeg/FFprobe version calls, and the
exact launcher's `--verify-runtime` branch pass. Models, libraries and license notices are unchanged.

The previous complete package is recoverable from
`artifacts/phase1-multisource/package-cached-proxy-fb5c94d/previous-package.zip`
(SHA-256 `FC49684B3474D7BB98E4CB27BB887BC3B142E9324D0013B566543887347D51DF`).
The adjacent `verification.json` records file/import hashes and the check scope. No binaries or
private models were pushed. `smoke_status` stays `not_run`: local static checks do not establish
GUI behavior, dynamic GPU-library loading, clean-host compatibility, or windowed performance.
