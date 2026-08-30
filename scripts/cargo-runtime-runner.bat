@echo off
setlocal

set "MAELSTROM_REPO_ROOT=%~dp0.."
set "MAELSTROM_FFMPEG_BIN=%MAELSTROM_REPO_ROOT%\.deps\ffmpeg-project-8.1\bin"

set "MAELSTROM_RUNTIME_MISSING="
for %%D in (
    avcodec-62.dll
    avdevice-62.dll
    avfilter-11.dll
    avformat-62.dll
    avutil-60.dll
    swresample-6.dll
    swscale-9.dll
    libgcc_s_seh-1.dll
    libstdc++-6.dll
    libvpl.dll
    libwinpthread-1.dll
) do if not exist "%MAELSTROM_FFMPEG_BIN%\%%D" (
    echo Missing project-local runtime: %%D
    set "MAELSTROM_RUNTIME_MISSING=1"
)

if defined MAELSTROM_RUNTIME_MISSING (
    echo.
    echo Maelstrom's project-local FFmpeg runtime is incomplete:
    echo %MAELSTROM_FFMPEG_BIN%
    echo Rebuild the pinned dependency bundle; do not install DLLs into Windows.
    exit /b 1
)

set "PATH=%MAELSTROM_FFMPEG_BIN%;%PATH%"
%*
exit /b %ERRORLEVEL%
