#requires -Version 7.0
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateNotNullOrEmpty()]
    [string]$MediaPath,
    [string]$FfmpegRoot,
    [string]$ReportPath,
    [ValidateRange(3, 100)]
    [int]$HistoryTrials = 10
)

$ErrorActionPreference = 'Stop'
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
if ([string]::IsNullOrWhiteSpace($FfmpegRoot)) {
    $FfmpegRoot = Join-Path $repoRoot '.deps\ffmpeg-project-8.1'
}
if ([string]::IsNullOrWhiteSpace($ReportPath)) {
    $ReportPath = Join-Path $repoRoot 'artifacts\phase0-foundation\timeline-foundation.json'
}

function Test-AbsolutePath([string]$Path) {
    return [IO.Path]::IsPathRooted($Path) -and
        [string]::Equals([IO.Path]::GetFullPath($Path), $Path, [StringComparison]::OrdinalIgnoreCase)
}

function Convert-TimeToMilliseconds([string]$Value) {
    if ($Value -notmatch '^(?<number>[0-9.]+)(?<unit>ns|µs|ms|s)$') {
        throw "Could not parse duration: $Value"
    }
    $number = [double]$Matches.number
    switch ($Matches.unit) {
        'ns' { return $number / 1000000.0 }
        'µs' { return $number / 1000.0 }
        'ms' { return $number }
        's' { return $number * 1000.0 }
    }
}

function Get-NearestRank([double[]]$Values, [double]$Percentile) {
    $sorted = @($Values | Sort-Object)
    $index = [Math]::Max(0, [Math]::Ceiling($Percentile * $sorted.Count) - 1)
    return [double]$sorted[$index]
}

function Invoke-CargoTest([string]$Label, [string[]]$Arguments) {
    $output = @(& $script:cargo @Arguments 2>&1)
    if ($LASTEXITCODE -ne 0) {
        $tail = ($output | Select-Object -Last 40) -join [Environment]::NewLine
        throw "$Label failed with exit code $LASTEXITCODE`n$tail"
    }
    return ,$output
}

if (-not (Test-AbsolutePath $MediaPath)) { throw 'MediaPath must be an absolute path.' }
if (-not (Test-Path -LiteralPath $MediaPath -PathType Leaf)) { throw "MediaPath is missing: $MediaPath" }
if (-not (Test-AbsolutePath $FfmpegRoot)) { throw 'FfmpegRoot must be an absolute path.' }
$ffmpegRootPath = (Resolve-Path -LiteralPath $FfmpegRoot).Path
$ffmpeg = Join-Path $ffmpegRootPath 'bin\ffmpeg.exe'
$ffprobe = Join-Path $ffmpegRootPath 'bin\ffprobe.exe'
foreach ($tool in $ffmpeg, $ffprobe) {
    if (-not (Test-Path -LiteralPath $tool -PathType Leaf)) { throw "Pinned media tool is missing: $tool" }
}

$artifactRoot = [IO.Path]::GetFullPath((Join-Path $repoRoot 'artifacts\phase0-foundation'))
$resolvedReportPath = if ([IO.Path]::IsPathRooted($ReportPath)) {
    [IO.Path]::GetFullPath($ReportPath)
} else {
    [IO.Path]::GetFullPath((Join-Path $repoRoot $ReportPath))
}
if (-not [string]::Equals([IO.Path]::GetDirectoryName($resolvedReportPath), $artifactRoot, [StringComparison]::OrdinalIgnoreCase) -or
    [IO.Path]::GetExtension($resolvedReportPath) -ne '.json') {
    throw "ReportPath must be a JSON file directly inside $artifactRoot"
}

$cargoCommand = Get-Command cargo.exe -CommandType Application -ErrorAction Stop
$script:cargo = [IO.Path]::GetFullPath($cargoCommand.Source)
if (-not (Test-AbsolutePath $script:cargo)) { throw 'Cargo did not resolve to an absolute executable path.' }
$gitCommand = Get-Command git.exe -CommandType Application -ErrorAction Stop
$git = [IO.Path]::GetFullPath($gitCommand.Source)
if (-not (Test-AbsolutePath $git)) { throw 'Git did not resolve to an absolute executable path.' }

$sourceCommit = (& $git -C $repoRoot rev-parse HEAD).Trim()
if ($LASTEXITCODE -ne 0 -or $sourceCommit -notmatch '^[0-9a-f]{40}$') { throw 'Could not resolve the source commit.' }
$trackedChanges = @(& $git -C $repoRoot status --porcelain --untracked-files=no)
if ($LASTEXITCODE -ne 0 -or $trackedChanges.Count -ne 0) {
    throw 'Timeline foundation evidence requires a clean tracked source tree.'
}

$probeOutput = @(& $ffprobe -v error -select_streams v:0 -show_entries 'stream=codec_name,width,height,avg_frame_rate,duration' -of json $MediaPath 2>&1)
if ($LASTEXITCODE -ne 0) { throw "FFprobe could not inspect the supplied media: $($probeOutput -join [Environment]::NewLine)" }
$probe = ($probeOutput -join [Environment]::NewLine) | ConvertFrom-Json
$stream = @($probe.streams)[0]
if ($null -eq $stream -or $stream.codec_name -ne 'h264' -or [int]$stream.width -lt 1 -or [int]$stream.height -lt 1 -or [double]$stream.duration -le 0) {
    throw 'Timeline foundation media must contain a positive-duration H.264 video stream.'
}
$media = Get-Item -LiteralPath $MediaPath
$ffprobeVersion = @(& $ffprobe -hide_banner -version 2>&1)
if ($LASTEXITCODE -ne 0 -or $ffprobeVersion.Count -lt 1) { throw 'Could not identify the pinned FFprobe runtime.' }

$savedFfmpeg = $env:FFMPEG_DIR
$savedLibclang = $env:LIBCLANG_PATH
$startedAt = [DateTime]::UtcNow
$stopwatch = [Diagnostics.Stopwatch]::StartNew()
$temporaryReport = "$resolvedReportPath.tmp-$PID"
try {
    $env:FFMPEG_DIR = $ffmpegRootPath
    if ([string]::IsNullOrWhiteSpace($env:LIBCLANG_PATH)) {
        $env:LIBCLANG_PATH = Join-Path $repoRoot '.deps\libclang-bindgen'
    }

    $historyRows = @()
    for ($trial = 1; $trial -le $HistoryTrials; $trial++) {
        $output = Invoke-CargoTest '50,000-clip history gate' @(
            'test', '-p', 'nle-ui-core', '--release', '--lib',
            'editor::tests::fifty_thousand_clip_editor_history_events_stay_under_two_ms',
            '--', '--ignored', '--exact', '--nocapture'
        )
        $line = ($output | Select-String -SimpleMatch '50k editor history events:' | Select-Object -Last 1).Line
        if ($line -notmatch 'press checkpoint=(?<press>[0-9.]+(?:ns|µs|ms|s)), move-only=(?<move>[0-9.]+(?:ns|µs|ms|s)), history-record=(?<history>[0-9.]+(?:ns|µs|ms|s)), edit\+release=(?<release>[0-9.]+(?:ns|µs|ms|s))') {
            throw "Could not parse history timing trial ${trial}: $line"
        }
        $historyRows += [ordered]@{
            trial = $trial
            press_ms = Convert-TimeToMilliseconds $Matches.press
            move_ms = Convert-TimeToMilliseconds $Matches.move
            history_record_ms = Convert-TimeToMilliseconds $Matches.history
            edit_release_ms = Convert-TimeToMilliseconds $Matches.release
        }
    }

    $uiOutput = Invoke-CargoTest '50,000-clip UI CPU gate' @(
        'test', '-p', 'nle-ui-core', '--release', '--test', 'timeline_performance',
        'fifty_thousand_clip_editor_cpu_evidence', '--', '--ignored', '--exact', '--nocapture'
    )
    $uiLine = ($uiOutput | Select-String -SimpleMatch 'timeline 50k CPU evidence:' | Select-Object -Last 1).Line
    if ($uiLine -notmatch 'wide p50=(?<wp50>[0-9.]+(?:ns|µs|ms|s)) p95=(?<wp95>[0-9.]+(?:ns|µs|ms|s)) primitives=(?<wide>[0-9]+); detail p50=(?<dp50>[0-9.]+(?:ns|µs|ms|s)) p95=(?<dp95>[0-9.]+(?:ns|µs|ms|s)) primitives=(?<detail>[0-9]+); playhead p50=(?<pp50>[0-9.]+(?:ns|µs|ms|s)) p95=(?<pp95>[0-9.]+(?:ns|µs|ms|s))') {
        throw "Could not parse 50,000-clip UI timing: $uiLine"
    }
    $ui = [ordered]@{
        wide_p50_ms = Convert-TimeToMilliseconds $Matches.wp50
        wide_p95_ms = Convert-TimeToMilliseconds $Matches.wp95
        wide_primitives = [int]$Matches.wide
        detail_p50_ms = Convert-TimeToMilliseconds $Matches.dp50
        detail_p95_ms = Convert-TimeToMilliseconds $Matches.dp95
        detail_primitives = [int]$Matches.detail
        playhead_p50_ms = Convert-TimeToMilliseconds $Matches.pp50
        playhead_p95_ms = Convert-TimeToMilliseconds $Matches.pp95
    }

    $savedMedia = $env:MAELSTROM_TEST_MEDIA
    try {
        $env:MAELSTROM_TEST_MEDIA = $MediaPath
        $combinedOutput = Invoke-CargoTest 'H.264 plus 20,000-bar foundation gate' @(
            'test', '-p', 'nle-ui-core', '--release', '--test', 'timeline_performance',
            'real_h264_scrub_stays_responsive_with_twenty_thousand_bars',
            '--', '--ignored', '--exact', '--nocapture'
        )
    } finally {
        if ($null -eq $savedMedia) { Remove-Item Env:MAELSTROM_TEST_MEDIA -ErrorAction SilentlyContinue }
        else { $env:MAELSTROM_TEST_MEDIA = $savedMedia }
    }
    $combinedLine = ($combinedOutput | Select-String -SimpleMatch 'combined foundation evidence:' | Select-Object -Last 1).Line
    if ($combinedLine -notmatch 'bars=(?<bars>[0-9]+), UI p50=(?<p50>[0-9.]+(?:ns|µs|ms|s)), p95=(?<p95>[0-9.]+(?:ns|µs|ms|s)), primitives=(?<primitives>[0-9]+), decoded=(?<decoded>[0-9]+)us via (?<backend>.+)$') {
        throw "Could not parse combined foundation timing: $combinedLine"
    }
    $combined = [ordered]@{
        bars = [int]$Matches.bars
        ui_p50_ms = Convert-TimeToMilliseconds $Matches.p50
        ui_p95_ms = Convert-TimeToMilliseconds $Matches.p95
        primitives = [int]$Matches.primitives
        decoded_source_tick_us = [int64]$Matches.decoded
        decoder_backend = $Matches.backend.Trim()
    }

    $endCommit = (& $git -C $repoRoot rev-parse HEAD).Trim()
    $endChanges = @(& $git -C $repoRoot status --porcelain --untracked-files=no)
    if ($LASTEXITCODE -ne 0 -or $endCommit -ne $sourceCommit -or $endChanges.Count -ne 0) {
        throw 'Tracked source changed during timeline foundation measurement.'
    }

    $stopwatch.Stop()
    $report = [ordered]@{
        schema_version = 1
        status = 'passed'
        generated_at_utc = [DateTime]::UtcNow.ToString('o')
        elapsed_seconds = [Math]::Round($stopwatch.Elapsed.TotalSeconds, 6)
        source_commit = $sourceCommit
        tracked_tree_clean = $true
        scope = 'Headless release CPU timing and real H.264 decode acceptance; not GUI-present, GPU completion, physical input latency, scanout, package smoke, or cross-hardware proof.'
        history = [ordered]@{
            trial_count = $HistoryTrials
            limit_ms = 2.0
            trials = $historyRows
            press_p50_ms = Get-NearestRank $historyRows.press_ms 0.50
            press_p95_ms = Get-NearestRank $historyRows.press_ms 0.95
            edit_release_p50_ms = Get-NearestRank $historyRows.edit_release_ms 0.50
            edit_release_p95_ms = Get-NearestRank $historyRows.edit_release_ms 0.95
        }
        ui = $ui
        combined = $combined
        media = [ordered]@{
            sha256 = (Get-FileHash -LiteralPath $media.FullName -Algorithm SHA256).Hash
            bytes = $media.Length
            codec = $stream.codec_name
            width = [int]$stream.width
            height = [int]$stream.height
            average_frame_rate = $stream.avg_frame_rate
            duration_seconds = [double]$stream.duration
        }
        tools = [ordered]@{
            cargo = $script:cargo
            ffprobe_version = [string]$ffprobeVersion[0]
        }
    }
    New-Item -ItemType Directory -Force -Path $artifactRoot | Out-Null
    $json = $report | ConvertTo-Json -Depth 8
    [IO.File]::WriteAllText($temporaryReport, $json, [Text.UTF8Encoding]::new($false))
    Move-Item -LiteralPath $temporaryReport -Destination $resolvedReportPath -Force
    Write-Output "Timeline foundation: PASS ($resolvedReportPath)"
    Write-Output ("History press/release p95: {0:N4}/{1:N4} ms; UI wide/detail/playhead p95: {2:N4}/{3:N4}/{4:N4} ms; combined p95: {5:N4} ms" -f
        $report.history.press_p95_ms, $report.history.edit_release_p95_ms,
        $ui.wide_p95_ms, $ui.detail_p95_ms, $ui.playhead_p95_ms, $combined.ui_p95_ms)
} finally {
    if ($null -eq $savedFfmpeg) { Remove-Item Env:FFMPEG_DIR -ErrorAction SilentlyContinue }
    else { $env:FFMPEG_DIR = $savedFfmpeg }
    if ($null -eq $savedLibclang) { Remove-Item Env:LIBCLANG_PATH -ErrorAction SilentlyContinue }
    else { $env:LIBCLANG_PATH = $savedLibclang }
    if (Test-Path -LiteralPath $temporaryReport) { Remove-Item -LiteralPath $temporaryReport -Force }
}
