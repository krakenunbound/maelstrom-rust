# nle-decode

`nle-decode` is Maelstrom's narrow monitor-frame decoder. `MonitorDecoder`
owns one background libav scheduler, coalesces seeks to the latest target, and
retains demuxer/decoder contexts per media ID. Nearby forward targets continue
through the existing decoder; backward or distant targets seek, flush, and
preroll in that same context. It returns only the newest bounded RGBA8 frame
through a nonblocking slot.

Live monitor playback and scrubbing never start an `ffmpeg.exe` process and
never read a thumbnail or frame cache. The CLI appears only in tests that
generate temporary media fixtures.
