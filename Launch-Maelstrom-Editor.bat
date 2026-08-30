@echo off
setlocal

set "MAELSTROM_EDITOR_DIR=H:\Maelstrom Rust\dist\Maelstrom-Windows-x64"
set "MAELSTROM_EDITOR_EXE=H:\Maelstrom Rust\dist\Maelstrom-Windows-x64\Maelstrom.exe"
set "KRAKEN_RTX_VSR_DIR=H:\Maelstrom Restricted Assets\rtx-vsr"

if not exist "%MAELSTROM_EDITOR_EXE%" (
    echo Maelstrom editor was not found at:
    echo %MAELSTROM_EDITOR_EXE%
    pause
    exit /b 1
)

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
    vcruntime140.dll
) do if not exist "%MAELSTROM_EDITOR_DIR%\%%D" (
    echo Missing required runtime: %%D
    set "MAELSTROM_RUNTIME_MISSING=1"
)

if defined MAELSTROM_RUNTIME_MISSING (
    echo.
    echo The packaged editor runtime is incomplete. Rebuild the package instead of
    echo installing DLL files into Windows.
    pause
    exit /b 1
)

if not exist "%KRAKEN_RTX_VSR_DIR%\NVVideoEffects.dll" (
    echo Optional NVIDIA RTX VSR runtime was not found at:
    echo %KRAKEN_RTX_VSR_DIR%
    echo Maelstrom will start, but Kraken Upscale will be unavailable.
)

if /I "%~1"=="--verify-runtime" (
    echo Maelstrom packaged runtime is complete:
    echo %MAELSTROM_EDITOR_DIR%
    exit /b 0
)

set "PATH=%MAELSTROM_EDITOR_DIR%;%PATH%"
rem Opt-in qualification runners retain this exact launcher as the process-tree root.
if /I "%MAELSTROM_LAUNCHER_WAIT%"=="1" (
    start "Maelstrom Video Editor" /wait /D "%MAELSTROM_EDITOR_DIR%" "%MAELSTROM_EDITOR_EXE%" %*
    exit /b
)
start "Maelstrom Video Editor" /D "%MAELSTROM_EDITOR_DIR%" "%MAELSTROM_EDITOR_EXE%" %*
