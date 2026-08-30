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
one fixed profile. Queuing, persistent proxy attachment, and more profiles are later work.
