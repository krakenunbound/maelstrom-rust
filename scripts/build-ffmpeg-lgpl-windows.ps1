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
$manifest = Join-Path $outputPath 'BUILD-MANIFEST.txt'
$aomLicense = Join-Path $outputPath 'libaom-LICENSE.txt'
$aomPatents = Join-Path $outputPath 'libaom-PATENTS.txt'
if (-not (Test-Path -LiteralPath $ffmpeg)) {
    throw 'The build did not produce bin\ffmpeg.exe.'
}
if (-not (Test-Path -LiteralPath $manifest -PathType Leaf) -or -not (Test-Path -LiteralPath $aomLicense -PathType Leaf) -or -not (Test-Path -LiteralPath $aomPatents -PathType Leaf)) {
    throw 'The build did not produce the required pinned libaom manifest and license artifacts.'
}
$manifestText = Get-Content -LiteralPath $manifest -Raw
if ($manifestText -notmatch 'libaom commit: d9c115ce0951324dee243041ef810e27202de20f \(tag v3\.13\.0; decoder-only static\)') {
    throw 'The FFmpeg build manifest does not identify the pinned decoder-only libaom source.'
}
if (Get-ChildItem -LiteralPath (Join-Path $outputPath 'bin') -Filter 'libaom*.dll' -ErrorAction SilentlyContinue) {
    throw 'The static libaom build unexpectedly emitted a libaom DLL.'
}
$configuration = (& $ffmpeg -hide_banner -version 2>&1 | Out-String)
if ($configuration -match '--enable-gpl' -or $configuration -match '--enable-nonfree' -or $configuration -notmatch '--enable-libaom') {
    throw 'The project-owned FFmpeg build is missing libaom or unexpectedly enabled GPL/nonfree components.'
}
$decoderInventory = (& $ffmpeg -hide_banner -decoders 2>&1 | Out-String)
if ($LASTEXITCODE -ne 0 -or $decoderInventory -notmatch '\blibaom[-_]av1\b') {
    throw 'The project-owned FFmpeg build does not expose the libaom AV1 decoder.'
}
$encoderInventory = (& $ffmpeg -hide_banner -encoders 2>&1 | Out-String)
if ($LASTEXITCODE -ne 0 -or $encoderInventory -match '\blibaom[-_]av1\b') {
    throw 'The project-owned FFmpeg build unexpectedly exposes the disabled libaom AV1 encoder.'
}
Get-Item -LiteralPath $outputPath
