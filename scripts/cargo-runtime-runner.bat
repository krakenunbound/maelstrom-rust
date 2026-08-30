@echo off
setlocal

for %%I in ("%~dp0..") do set "MAELSTROM_REPO_ROOT=%%~fI"
set "MAELSTROM_FFMPEG_BIN=%MAELSTROM_REPO_ROOT%\.deps\ffmpeg-project-8.1\bin"
set "MAELSTROM_PACKAGE_DIR=%MAELSTROM_REPO_ROOT%\dist\Maelstrom-Windows-x64"

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

set "MAELSTROM_VC_RUNTIME_DIR="
if exist "%MAELSTROM_PACKAGE_DIR%\vcruntime140.dll" (
    set "MAELSTROM_VC_RUNTIME_DIR=%MAELSTROM_PACKAGE_DIR%"
) else (
    if not exist "%SystemRoot%\System32\vcruntime140.dll" (
        echo Missing Microsoft Visual C++ runtime: vcruntime140.dll
        echo Rebuild Maelstrom's Windows package or install Microsoft's official x64
        echo Visual C++ Redistributable. Do not download individual DLLs from third-party sites.
        exit /b 1
    )
)

if defined MAELSTROM_VC_RUNTIME_DIR set "PATH=%MAELSTROM_VC_RUNTIME_DIR%;%PATH%"
set "PATH=%MAELSTROM_FFMPEG_BIN%;%PATH%"

if /I "%~1"=="--verify-runtime" (
    echo Maelstrom developer runtime is complete:
    echo %MAELSTROM_FFMPEG_BIN%
    if defined MAELSTROM_VC_RUNTIME_DIR echo %MAELSTROM_VC_RUNTIME_DIR%
    exit /b 0
)

%*
exit /b %ERRORLEVEL%
