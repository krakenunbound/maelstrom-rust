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

if not exist "%KRAKEN_RTX_VSR_DIR%\NVVideoEffects.dll" (
    echo Optional NVIDIA RTX VSR runtime was not found at:
    echo %KRAKEN_RTX_VSR_DIR%
    echo Maelstrom will start, but Kraken Upscale will be unavailable.
)

start "Maelstrom Video Editor" /D "%MAELSTROM_EDITOR_DIR%" "%MAELSTROM_EDITOR_EXE%" %*
