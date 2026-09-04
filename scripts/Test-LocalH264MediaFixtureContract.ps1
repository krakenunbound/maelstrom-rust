#requires -Version 7.0
[CmdletBinding()]
param(
    [string]$FfmpegRoot = $env:FFMPEG_DIR,
    [string]$SourcePath
)

$ErrorActionPreference = 'Stop'
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$validator = Join-Path $PSScriptRoot 'Test-MediaFixtures.ps1'
if ([string]::IsNullOrWhiteSpace($FfmpegRoot)) { $FfmpegRoot = Join-Path $repoRoot '.deps\ffmpeg-project-8.1' }
if ([string]::IsNullOrWhiteSpace($SourcePath)) { $SourcePath = Join-Path $repoRoot 'artifacts\phase1-multisource\scrub-seek-open-gop-qsv-h264.mp4' }
if (-not (Test-Path -LiteralPath $SourcePath -PathType Leaf)) {
    Write-Output 'Local H.264 media fixture contract: SKIP (source unavailable)'
    exit 0
}

$previousCorpusRoot = $env:MAELSTROM_REAL_MEDIA_ROOT

try {
    try { $env:MAELSTROM_REAL_MEDIA_ROOT = (Split-Path -Parent (Resolve-Path -LiteralPath $SourcePath).Path) }
    catch { throw 'Local H.264 media fixture contract: source unavailable.' }
    & $validator -FfmpegRoot $FfmpegRoot -IncludeRealCorpus
    if ($LASTEXITCODE -ne 0) { throw 'Local H.264 media fixture contract positive validation failed.' }
    Write-Output 'Local H.264 media fixture contract: PASS (in-place positive validation)'
}
finally {
    if ($null -eq $previousCorpusRoot) { [Environment]::SetEnvironmentVariable('MAELSTROM_REAL_MEDIA_ROOT', $null, 'Process') }
    else { $env:MAELSTROM_REAL_MEDIA_ROOT = $previousCorpusRoot }
}
