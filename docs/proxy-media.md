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
- Proxy files, cache paths, fingerprints, and runtime readiness are never serialized into
  `.nleproj`; reopening a project never depends on a derived file. Project-document version 9
  stores only the user's per-media enable preference.
- When timeline video is analyzed after placement or project reopen, the existing analysis worker
  rediscovers a matching completed proxy in the local cache. A project with no saved preference
  shows **Proxy ready** and retains original media. A saved enable preference starts background
  validation and retains original media until the current artifact passes every guard.
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
one fixed profile. Local cache discovery survives reopening without storing a derived dependency in
the project; only user intent persists. Portable/external proxy attachments, generation queues, and
more profiles remain later work.

## Durable enable preference — 2026-08-31

Project-document version 9 adds a defaulted `proxy_enabled` boolean to each durable media reference.
Versions 1–8 remain readable and migrate with every preference disabled. Older builds reject version
9 at the existing version preflight rather than partially parsing an unknown field. Exporting or
duplicating a project preserves the preference, while proxy artifacts remain local derived cache.

The application owns a separate desired-ID set. Explicit enable saves intent immediately; disable
or delete clears it. Project switching clears the session copy, and reopening restores it from the
new document before media analysis. Cache discovery installs a disabled runtime record and queues
validation only when desired. The monitor and audio/export routes continue using original media
until the current artifact, source fingerprint, media path, project ticket, and validation result
all agree. Missing, stale, relinked, corrupt, deleted, or late proxy results cannot activate a route;
temporary failure does not erase the user's saved choice.

Schema round trips, v8 migration, current/future-version handling, autosave, project reopen,
checking-before-activation, explicit-disable precedence, relink guards, and project-switch reset are
covered headlessly. This changes persistence and routing intent only; it adds no proxy path, codec,
profile, dependency, or GUI-thread filesystem work.

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
bound or whole-workflow nonblocking guarantee. Completion/enable/reconciliation checks are moved
in the subsequent checkpoint below; the app's tool-path resolver and synchronous generation/
deletion cancellation/reset teardown remain separate lifecycle work. No image-quality, codec,
frame-rate, cache-cap or dependency change.

## Background activation and reconciliation checkpoint — 2026-08-31

Completed generation and explicit enable now submit identity/metadata checks to one lazy, owned
validation worker. The interface shows **Checking proxy** / **プロキシを確認中** and keeps original
media active until the matching result succeeds. The nested menu retains Use Original and Delete
while checking. No source path, audio/export route, saved project state, or playback-resolution
default changes. The worker checks the current profile, nonzero bounded output size, regular-file
metadata, recorded output size and source fingerprint; it does not claim to validate full decoding.

Request and result channels each hold at most 64 entries; the app also caps pending tickets at 64.
Submission and polling are nonblocking. A full activation queue leaves the original selected and
asks the user to retry. Passive reconciliation retains the existing route while checking. A recheck
bit on each existing record carries deferred cache checks across a full queue without allocating a
second request backlog. Another cache mutation during an in-flight check requests a subsequent
check. Result polling is capped at 64 per event and batches monitor refresh after route changes.

Tickets are not reused at project reset. Ticket, artifact and requested-path guards reject late
replies after user switches, delete, relink or replacement. Decoder failure invalidates a pending
check before falling back. A generation cancellation flag also rejects a completion that was already
queued when the user cancelled. Reset invalidates the validation worker's epoch without joining it;
queued obsolete work skips filesystem inspection and an in-flight obsolete result is discarded.
The worker remains owned until final shutdown. Dropping its result receiver before joining unblocks
a full publication queue; an OS filesystem call already in progress can still delay final shutdown.

Two regressions fail before their fixes: activation must remain on the original until validation,
and reconciliation must not skip stale records when the queue is full. Four worker tests cover
bounded blocked-worker submission, old-epoch suppression, metadata rejection and full-result-queue
shutdown. Ten additional app tests cover pending status, missing files, user overrides, relink/
replacement, reset, delete, queued generation completion/cancellation, capacity, passive fallback,
and cache mutation while checks are in flight. Existing real generated-proxy reopen/enable and
decode-failure tests exercise the asynchronous path; the EN/JA UI status remains session-only.

Final verification: 800 release workspace tests pass (24 opt-in tests ignored), strict all-target
release Clippy and formatting pass, seven fixture contracts and seven Phase 0 scenarios pass.
Both opt-in real-media proxy tests also pass. Before/after logs and hash evidence use
`proxy-async-validation-` in ignored `artifacts/phase1-multisource/`; final scenario evidence is
`artifacts/phase0-scenarios/proxy-async-validation-final-scenarios.json`.
Parent review found and repaired the full-cache edge case. Independent review was unavailable
(agent thread limit). These are worker/state/real-media gates, not a measured GUI-latency bound,
cross-hardware guarantee, or closure of the remaining tool-resolution/teardown work.

## Portable package checkpoint

Built from clean commit `9a17a2c2c75cd6942912796118abe46418140fb4` without opening the editor,
including cache rediscovery, asynchronous validation, and nonblocking cancellation/reset.
The executable SHA-256 is `F61C4CA8A840478BA9364F1AE59BEC615C0EEFA81E6DC56413AF6C2219D08F4A`,
matching the release build and package-status record. All 23 files are inventoried; only the
executable/status differ from the previous package. Thirteen pinned runtime hashes, the authorized
VC runtime copy, fifteen AMD64/static-import inventories, FFmpeg/FFprobe version calls, and the
exact launcher's `--verify-runtime` branch pass. Models, libraries and license notices are unchanged.

The previous complete package is recoverable from
`artifacts/phase1-multisource/package-proxy-lifecycle-9a17a2c/previous-package.zip`
(SHA-256 `FC466DE83F4023619AFE3A0AA8631E3B10C314DA30E83B8F3E7D648C65225EEF`).
The adjacent `verification.json` records file/import hashes and the check scope. No binaries or
private models were pushed. `smoke_status` stays `not_run`: local static checks do not establish
GUI behavior, dynamic GPU-library loading, clean-host compatibility, or windowed performance.


## Nonblocking cancellation and teardown checkpoint — 2026-08-31

Generation cancellation is now an atomic signal only: interface actions never take a child-process
lock, kill a process, or join an in-flight proxy worker. The proxy worker exclusively owns FFprobe/
FFmpeg and polls cancellation every 10 ms while either subprocess is running. One scoped reader
drains each output pipe. Progress holds at most eight 1 KiB lines; diagnostics retain only their
latest 64 KiB. The app-facing event queue holds 64 entries, discards redundant progress when full,
and preserves terminal completion/cancellation. Final job Drop releases the receiver before joining
so a terminal publisher cannot deadlock behind a full progress queue.

Cancel/reset clears project and media identities immediately but retains exactly one generation and
one deletion owner until each reports finished. No obsolete event can update the new project. New
proxy cache mutations are refused with an EN/JA retry message while either relevant cleanup slot is
still occupied. Idle windows poll those slots at a bounded 20 ms cadence, closing the race between a
terminal notification and thread exit; a finished handle is the only handle joined by UI polling.
Application shutdown signals both jobs before other flushes, then normal field ownership joins them.

One before-failing regression exercises generation/deletion polling and reset with a worker held
after terminal publication. It proves all four actions return before releasing that worker, retained
slots reject overlapping work, late state stays unchanged, and both slots are eventually released.
Process tests cover silent and diagnostic-flood children, bounded pipe data, bounded terminal events,
and receiver-first Drop. Existing cancellation/source-change cases and both real FFmpeg proxy gates
continue to pass.

Verification: 804 release workspace tests pass (24 opt-in tests ignored), plus both explicitly
enabled real-media proxy tests. Strict all-target release Clippy and formatting, seven fixture
contracts, and seven Phase 0 scenarios pass. Evidence uses the `proxy-lifecycle-` prefix under ignored
artifacts. Parent-reviewed; independent agent unavailable (agent thread limit). No editor was
launched. This establishes nonblocking UI cancellation/reset ownership, not bounded OS filesystem
I/O: metadata, cache enumeration/removal, and final application shutdown can still wait on the OS.
Shared runtime-tool discovery is now app-owned startup work. Quick Export, Kraken Upscale, and
proxy generation reuse one complete canonical FFmpeg/FFprobe pair and never fall back to ambient
`PATH`; see [`runtime-media-tools.md`](runtime-media-tools.md). Final shutdown can still wait for an
OS filesystem call already in progress, so this does not claim bounded filesystem cancellation.
