# Maelstrom workspace rules

## Executable and runtime safety

- Never run `Maelstrom.exe`, `nle-app.exe`, or a generated `target\**\*.exe`
  directly. In particular, never execute hashed Cargo test binaries such as
  `target\debug\deps\nle_app-<hash>.exe`; they bypass the project-local FFmpeg
  runtime and open misleading Windows missing-DLL dialogs.
- Launch the editor only when the user explicitly requests it, and only through
  the exact full path `H:\Maelstrom Rust\Launch-Maelstrom-Editor.bat`.
- Run Rust tests through
  `C:\Users\The Kraken\.cargo\bin\cargo.exe test ...` from this workspace.
  `.cargo\config.toml` routes test binaries through
  `H:\Maelstrom Rust\scripts\cargo-runtime-runner.bat`, which supplies the
  project-local runtime.
- Do not install or download individual DLL files. The supported adjacent
  runtime is `H:\Maelstrom Rust\dist\Maelstrom-Windows-x64`; the approved
  developer FFmpeg bundle is
  `H:\Maelstrom Rust\.deps\ffmpeg-project-8.1\bin`.
- Before finishing, stop only the exact process IDs started during the task and
  confirm no Cargo, Rust compiler, test-harness, Maelstrom, FFmpeg, or FFprobe
  process was left behind.

## Long-running processes

Track every dev server, watcher, helper, benchmark, and other long-running
process by PID or tool session. Clean it up before finishing unless the user
explicitly asked to keep it running.
