[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateNotNullOrEmpty()]
    [string]$FfmpegRoot,
    [string]$ReportPath
)

$ErrorActionPreference = 'Stop'
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
if ([string]::IsNullOrWhiteSpace($ReportPath)) {
    $ReportPath = Join-Path $repoRoot 'artifacts\phase0-scenarios\phase0-scenarios.json'
}
function Test-AbsolutePath([string]$Path) {
    return [IO.Path]::IsPathRooted($Path) -and [string]::Equals([IO.Path]::GetFullPath($Path), $Path, [StringComparison]::OrdinalIgnoreCase)
}
if (-not (Test-AbsolutePath $FfmpegRoot)) { throw 'FFmpeg root must be an absolute path.' }
$ffmpegRootPath = (Resolve-Path -LiteralPath $FfmpegRoot).Path
$ffmpeg = Join-Path $ffmpegRootPath 'bin\ffmpeg.exe'
$ffprobe = Join-Path $ffmpegRootPath 'bin\ffprobe.exe'
if (-not (Test-Path -LiteralPath $ffmpeg -PathType Leaf) -or -not (Test-Path -LiteralPath $ffprobe -PathType Leaf)) {
    throw "Expected ffmpeg.exe and ffprobe.exe below $ffmpegRootPath\\bin."
}
$ffmpegVersion = & $ffmpeg -hide_banner -version 2>&1
if ($LASTEXITCODE -ne 0 -or $ffmpegVersion[0] -notmatch '^ffmpeg version n?8\.1(?:[.\s-]|$)') {
    throw "Phase 0 scenarios require the pinned FFmpeg 8.1 bundle: $ffmpegRootPath"
}

$artifactRoot = Join-Path $repoRoot 'artifacts\phase0-scenarios'
$resolvedArtifactRoot = [IO.Path]::GetFullPath($artifactRoot)
$resolvedReportPath = if ([IO.Path]::IsPathRooted($ReportPath)) {
    [IO.Path]::GetFullPath($ReportPath)
} else {
    [IO.Path]::GetFullPath((Join-Path $repoRoot $ReportPath))
}
if (-not [string]::Equals([IO.Path]::GetDirectoryName($resolvedReportPath), $resolvedArtifactRoot, [StringComparison]::OrdinalIgnoreCase)) {
    throw "Report output must be directly inside the ignored artifact directory: $resolvedArtifactRoot"
}
if ([IO.Path]::GetExtension($resolvedReportPath) -ne '.json') { throw 'Report output must be a JSON file.' }
New-Item -ItemType Directory -Force -Path $resolvedArtifactRoot | Out-Null

$fixtureRoot = Join-Path $repoRoot 'artifacts\media-fixtures'
$mediaPath = Join-Path $fixtureRoot 'bars-aac-2997.mp4'
$libclangRoot = if ([string]::IsNullOrWhiteSpace($env:LIBCLANG_PATH)) {
    Join-Path $repoRoot '.deps\libclang-bindgen'
} else {
    [IO.Path]::GetFullPath($env:LIBCLANG_PATH)
}
$libclang = Join-Path $libclangRoot 'libclang.dll'
if (-not (Test-Path -LiteralPath $libclang -PathType Leaf)) { throw "Missing local libclang required by native FFmpeg bindings: $libclang" }
$savedPath = $env:PATH
$savedFfmpeg = $env:FFMPEG_DIR
$savedLibclang = $env:LIBCLANG_PATH
$savedMedia = $env:MAELSTROM_PHASE0_MEDIA
$savedReport = $env:MAELSTROM_PHASE0_REPORT
$savedArtifactRoot = $env:MAELSTROM_PHASE0_ARTIFACT_ROOT
try {
    & (Join-Path $PSScriptRoot 'Generate-MediaFixtures.ps1') -FfmpegRoot $ffmpegRootPath
    & (Join-Path $PSScriptRoot 'Test-MediaFixtures.ps1') -FfmpegRoot $ffmpegRootPath -ArtifactRoot $fixtureRoot
    if (-not (Test-Path -LiteralPath $mediaPath -PathType Leaf)) { throw "Missing generated Phase 0 media fixture: $mediaPath" }

    Remove-Item -LiteralPath $resolvedReportPath -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath (Join-Path $resolvedArtifactRoot 'phase0-cancelled.mp4') -Force -ErrorAction SilentlyContinue
    $env:FFMPEG_DIR = $ffmpegRootPath
    $env:LIBCLANG_PATH = $libclangRoot
    $env:PATH = (Join-Path $ffmpegRootPath 'bin') + [IO.Path]::PathSeparator + $savedPath
    $env:MAELSTROM_PHASE0_MEDIA = $mediaPath
    $env:MAELSTROM_PHASE0_REPORT = $resolvedReportPath
    $env:MAELSTROM_PHASE0_ARTIFACT_ROOT = $resolvedArtifactRoot
    cargo test -p nle-app --release tests::phase0_scenario_matrix -- --ignored --exact --test-threads=1
    if ($LASTEXITCODE -ne 0) { throw 'Phase 0 scenario matrix failed.' }

    if (-not (Test-Path -LiteralPath $resolvedReportPath -PathType Leaf)) { throw 'Phase 0 scenario matrix did not write its report.' }
    $report = Get-Content -LiteralPath $resolvedReportPath -Raw | ConvertFrom-Json
    if ($report.schema_version -ne 1 -or $report.status -ne 'passed' -or @($report.scenarios).Count -ne 5) {
        throw 'Phase 0 scenario report has an unexpected schema, status, or scenario count.'
    }
    foreach ($scenario in @($report.scenarios)) {
        if ([string]::IsNullOrWhiteSpace($scenario.name) -or [int]$scenario.iterations -lt 1 -or [double]$scenario.elapsed_ms -lt 0 -or -not $scenario.passed -or [string]::IsNullOrWhiteSpace($scenario.evidence)) {
            throw "Invalid scenario evidence in report: $($scenario.name)"
        }
    }
    $memoryPressure = @($report.scenarios | Where-Object { $_.name -eq 'runtime_video_strip_cache_eviction' })
    if ($memoryPressure.Count -ne 1 -or [int]$memoryPressure[0].iterations -ne 5) {
        throw 'Phase 0 memory-pressure scenario is missing or has an unexpected iteration count.'
    }
    $evidence = [string]$memoryPressure[0].evidence
    if ($evidence -notmatch 'cumulative_bytes=(\d+) retained_bytes=(\d+) cap_bytes=(\d+) peak_live_bytes=(\d+)') {
        throw 'Phase 0 memory-pressure scenario does not report cumulative, retained, cap, and peak bytes.'
    }
    $cumulativeBytes = [int64]$Matches[1]
    $retainedBytes = [int64]$Matches[2]
    $capBytes = [int64]$Matches[3]
    $peakLiveBytes = [int64]$Matches[4]
    if ($cumulativeBytes -ne 367001600 -or $retainedBytes -ne 220200960 -or $capBytes -ne 268435456 -or $peakLiveBytes -ne 293601280 -or $retainedBytes -gt $capBytes -or $peakLiveBytes -le $capBytes) {
        throw "Phase 0 memory-pressure evidence is not the required bounded 70 MiB strip checkpoint: $evidence"
    }
    Write-Host "Phase 0 scenarios: PASS ($resolvedReportPath)"
}
finally {
    $env:PATH = $savedPath
    if ($null -eq $savedFfmpeg) { Remove-Item Env:FFMPEG_DIR -ErrorAction SilentlyContinue } else { $env:FFMPEG_DIR = $savedFfmpeg }
    if ($null -eq $savedLibclang) { Remove-Item Env:LIBCLANG_PATH -ErrorAction SilentlyContinue } else { $env:LIBCLANG_PATH = $savedLibclang }
    if ($null -eq $savedMedia) { Remove-Item Env:MAELSTROM_PHASE0_MEDIA -ErrorAction SilentlyContinue } else { $env:MAELSTROM_PHASE0_MEDIA = $savedMedia }
    if ($null -eq $savedReport) { Remove-Item Env:MAELSTROM_PHASE0_REPORT -ErrorAction SilentlyContinue } else { $env:MAELSTROM_PHASE0_REPORT = $savedReport }
    if ($null -eq $savedArtifactRoot) { Remove-Item Env:MAELSTROM_PHASE0_ARTIFACT_ROOT -ErrorAction SilentlyContinue } else { $env:MAELSTROM_PHASE0_ARTIFACT_ROOT = $savedArtifactRoot }
}
