# nle-waveform

Bounded waveform analysis for imported local media. `analyze_path` invokes
`ffprobe` first to identify the first audio stream, then invokes `ffmpeg` twice:
once to measure decoded mono frames and once to fill a fixed number of peak
bins. This keeps memory proportional to `target_bins`, rather than media
duration. The media path is passed to each child process as a direct argument;
no shell command is built or invoked.

**Never call this crate from the UI thread.** It is deliberately synchronous so
the application can schedule it on its existing background media-analysis
worker. It does not perform rendering, timeline mutation, playback, or audio
output.

`extract_video_strip` also produces a bounded, row-major RGBA preview atlas on
that worker. Short clips receive dense evenly spaced samples; consumers select
atlas cells during scrubbing without file access or a new decoder process.

This is the current process-backed FFmpeg seam. It is deliberately for
background media workers only. Sticky linked FFmpeg contexts will replace these
subprocesses when monitor playback is introduced. FFmpeg decides the available
codec/container coverage; unavailable executables, missing audio, bad probes,
and decode failures return an honest error instead of a fabricated waveform.
