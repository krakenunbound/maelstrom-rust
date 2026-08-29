[CmdletBinding()]
param(
    [string]$Output = '.deps\ffmpeg-project-8.1'
)

$ErrorActionPreference = 'Stop'
$repoRoot = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$outputPath = [System.IO.Path]::GetFullPath((Join-Path $repoRoot $Output))
$dependencyRoot = [System.IO.Path]::GetFullPath((Join-Path $repoRoot '.deps'))
$dependencyPrefix = $dependencyRoot.TrimEnd([System.IO.Path]::DirectorySeparatorChar) + [System.IO.Path]::DirectorySeparatorChar
if (-not $outputPath.StartsWith($dependencyPrefix, [System.StringComparison]::OrdinalIgnoreCase)) {
    throw "Refusing to write the FFmpeg build outside $dependencyRoot"
}
if ([System.IO.Path]::GetFileName($outputPath) -ne 'ffmpeg-project-8.1') {
    throw 'The reproducible build output must be the fixed .deps\ffmpeg-project-8.1 bundle.'
}

if (-not (Get-Command wsl.exe -ErrorAction SilentlyContinue)) {
    throw 'WSL2 is required for the pinned Windows cross-build.'
}
& wsl.exe -- bash -lc `
    'command -v git >/dev/null && command -v make >/dev/null && command -v cmake >/dev/null && command -v ninja >/dev/null && command -v nasm >/dev/null && command -v pkg-config >/dev/null && command -v x86_64-w64-mingw32-gcc >/dev/null && command -v x86_64-w64-mingw32-g++ >/dev/null && command -v x86_64-w64-mingw32-windres >/dev/null && command -v llvm-dlltool-19 >/dev/null'
if ($LASTEXITCODE -ne 0) {
    throw 'Missing WSL build prerequisites. Install: mingw-w64 make cmake ninja-build nasm pkg-config git ca-certificates llvm-19'
}

$scriptPath = & wsl.exe -- wslpath -a ($PSScriptRoot + '\build-ffmpeg-lgpl-windows.sh')
$wslOutput = & wsl.exe -- wslpath -a $outputPath
& wsl.exe -- bash $scriptPath $wslOutput
if ($LASTEXITCODE -ne 0) {
    throw 'The project-owned FFmpeg build failed.'
}

$ffmpeg = Join-Path $outputPath 'bin\ffmpeg.exe'
if (-not (Test-Path -LiteralPath $ffmpeg)) {
    throw 'The build did not produce bin\ffmpeg.exe.'
}
$configuration = (& $ffmpeg -hide_banner -version 2>&1 | Out-String)
if ($configuration -match '--enable-gpl' -or $configuration -match '--enable-nonfree') {
    throw 'The project-owned FFmpeg build unexpectedly enabled GPL/nonfree components.'
}
Get-Item -LiteralPath $outputPath
