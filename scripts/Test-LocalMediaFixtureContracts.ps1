#requires -Version 7.0
[CmdletBinding()]
param(
    [string]$FfmpegRoot = $env:FFMPEG_DIR,
    [string]$CorpusRoot
)

$ErrorActionPreference = 'Stop'
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$validator = Join-Path $PSScriptRoot 'Test-MediaFixtures.ps1'
$usesDefaultCorpusRoot = [string]::IsNullOrWhiteSpace($CorpusRoot)
if ($usesDefaultCorpusRoot) { $CorpusRoot = Join-Path $repoRoot 'artifacts\phase1-multisource' }
if ([string]::IsNullOrWhiteSpace($FfmpegRoot)) { $FfmpegRoot = Join-Path $repoRoot '.deps\ffmpeg-project-8.1' }
if (-not (Test-Path -LiteralPath $CorpusRoot -PathType Container)) {
    if ($usesDefaultCorpusRoot) {
        Write-Output 'Local media fixture contracts: SKIP (default corpus root unavailable)'
        exit 0
    }
    throw 'Local media fixture contracts: corpus root unavailable.'
}

$previousCorpusRoot = $env:MAELSTROM_REAL_MEDIA_ROOT
try {
    try { $env:MAELSTROM_REAL_MEDIA_ROOT = (Resolve-Path -LiteralPath $CorpusRoot).Path }
    catch { throw 'Local media fixture contracts: corpus root unavailable.' }
    & $validator -FfmpegRoot $FfmpegRoot -IncludeRealCorpus
    if ($LASTEXITCODE -ne 0) { throw 'Local media fixture contracts: positive validation failed.' }
    Write-Output 'Local media fixture contracts: PASS (in-place positive validation)'
}
finally {
    if ($null -eq $previousCorpusRoot) { [Environment]::SetEnvironmentVariable('MAELSTROM_REAL_MEDIA_ROOT', $null, 'Process') }
    else { $env:MAELSTROM_REAL_MEDIA_ROOT = $previousCorpusRoot }
}
