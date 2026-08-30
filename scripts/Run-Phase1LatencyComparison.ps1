[CmdletBinding()]
param([string]$ReportPath)

$ErrorActionPreference = 'Stop'

function Restore-EnvironmentValue {
    param([Parameter(Mandatory = $true)][string]$Name, $Value)
    if ($null -eq $Value) { Remove-Item "Env:$Name" -ErrorAction SilentlyContinue }
    else { Set-Item "Env:$Name" $Value }
}

function Test-Integer($Value) {
    $Value -is [byte] -or $Value -is [sbyte] -or $Value -is [int16] -or $Value -is [uint16] -or
        $Value -is [int32] -or $Value -is [uint32] -or $Value -is [int64] -or $Value -is [uint64]
}

function Test-FiniteNumber($Value) {
    if (Test-Integer $Value) { return $true }
    if ($Value -is [decimal]) { return $true }
    if ($Value -isnot [single] -and $Value -isnot [double]) { return $false }
    $doubleValue = [double]$Value
    return -not [double]::IsNaN($doubleValue) -and -not [double]::IsInfinity($doubleValue)
}

function Normalize-ExtendedPath([string]$Path) {
    if ($Path.StartsWith('\\?\')) { return $Path.Substring(4) }
    return $Path
}

function Assert-Unsigned($Object, [string]$Name, [string]$Context) {
    $property = $Object.PSObject.Properties[$Name]
    if ($null -eq $property -or -not (Test-Integer $property.Value) -or $property.Value -lt 0) {
        throw "$Context has invalid unsigned integer $Name."
    }
}

function Assert-Distribution($Distribution, [array]$Values, [string]$Context) {
    foreach ($name in @('p50', 'p95', 'max')) { Assert-Unsigned $Distribution $name $Context }
    if ($Values.Count -ne 20) { throw "$Context requires exactly 20 samples." }
    $sorted = @($Values | Sort-Object)
    $p50 = $sorted[[Math]::Ceiling($sorted.Count * .50) - 1]
    $p95 = $sorted[[Math]::Ceiling($sorted.Count * .95) - 1]
    if ($Distribution.p50 -ne $p50 -or $Distribution.p95 -ne $p95 -or $Distribution.max -ne $sorted[-1]) {
        throw "$Context nearest-rank summary does not match raw samples."
    }
}

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$fixtureRunner = Join-Path $PSScriptRoot 'Run-Phase1Multisource.ps1'
$fixtureRoot = [IO.Path]::GetFullPath((Join-Path $repoRoot 'artifacts\phase1-multisource'))
$artifactRoot = [IO.Path]::GetFullPath((Join-Path $repoRoot 'artifacts\phase1-latency'))
if ([string]::IsNullOrWhiteSpace($ReportPath)) { $ReportPath = Join-Path $artifactRoot 'phase1-latency-comparison.json' }
$resolvedReportPath = if ([IO.Path]::IsPathRooted($ReportPath)) { [IO.Path]::GetFullPath($ReportPath) } else { [IO.Path]::GetFullPath((Join-Path $repoRoot $ReportPath)) }
if ([IO.Path]::GetDirectoryName($resolvedReportPath) -ne $artifactRoot -or [IO.Path]::GetExtension($resolvedReportPath) -ine '.json') {
    throw "Report output must be a JSON file directly inside the ignored artifact directory: $artifactRoot"
}

# Reuse the exact pinned dynamic Full-1080p fixture seam. This calls cargo tests only; it does not
# launch the Maelstrom GUI or any application executable.
& $fixtureRunner
if ($LASTEXITCODE -ne 0) { throw 'Phase 1 fixture gate failed before latency comparison.' }
$fixtures = @(0, 60, 120, 180 | ForEach-Object { [IO.Path]::GetFullPath((Join-Path $fixtureRoot ("source-hue-{0:d3}.mp4" -f $_))) })
if (@($fixtures | Where-Object { -not (Test-Path -LiteralPath $_ -PathType Leaf) }).Count -ne 0) {
    throw 'The shared dynamic fixture runner did not produce all four expected sources.'
}

$ffmpegRoot = Join-Path $repoRoot '.deps\ffmpeg-project-8.1'
$libclangRoot = if ([string]::IsNullOrWhiteSpace($env:LIBCLANG_PATH)) {
    Join-Path $repoRoot '.deps\libclang-bindgen'
} else {
    [IO.Path]::GetFullPath($env:LIBCLANG_PATH)
}
if (-not (Test-Path -LiteralPath (Join-Path $libclangRoot 'libclang.dll') -PathType Leaf)) {
    throw "Missing libclang required by native FFmpeg bindings below: $libclangRoot"
}
$savedPath = $env:PATH; $savedFfmpeg = $env:FFMPEG_DIR; $savedLibclang = $env:LIBCLANG_PATH
$savedFirst = $env:MAELSTROM_TEST_MEDIA; $savedSecond = $env:MAELSTROM_TEST_MEDIA_SECOND
$savedThird = $env:MAELSTROM_TEST_MEDIA_THIRD; $savedFourth = $env:MAELSTROM_TEST_MEDIA_FOURTH
$savedReport = $env:MAELSTROM_PHASE1_LATENCY_REPORT
try {
    New-Item -ItemType Directory -Force -Path $artifactRoot | Out-Null
    Remove-Item -LiteralPath $resolvedReportPath -Force -ErrorAction SilentlyContinue
    $env:FFMPEG_DIR = $ffmpegRoot; $env:LIBCLANG_PATH = $libclangRoot
    $env:PATH = (Join-Path $ffmpegRoot 'bin') + [IO.Path]::PathSeparator + $libclangRoot + [IO.Path]::PathSeparator + $savedPath
    $env:MAELSTROM_TEST_MEDIA = $fixtures[0]; $env:MAELSTROM_TEST_MEDIA_SECOND = $fixtures[1]
    $env:MAELSTROM_TEST_MEDIA_THIRD = $fixtures[2]; $env:MAELSTROM_TEST_MEDIA_FOURTH = $fixtures[3]
    $env:MAELSTROM_PHASE1_LATENCY_REPORT = $resolvedReportPath
    cargo test -p nle-app --release tests::supplied_media_latency_comparison_uses_isolated_full_quality_trials -- --ignored --exact --test-threads=1
    $testExitCode = $LASTEXITCODE
    if (-not (Test-Path -LiteralPath $resolvedReportPath -PathType Leaf)) { throw 'Phase 1 latency comparison did not write its report.' }
    $report = Get-Content -LiteralPath $resolvedReportPath -Raw | ConvertFrom-Json
    foreach ($name in @('schema_version', 'trial_count_per_scenario', 'input_to_submit_p95_us_limit')) { Assert-Unsigned $report $name 'latency report' }
    if ($report.schema_version -ne 1 -or @('passed', 'failed') -notcontains $report.status -or $report.trial_count_per_scenario -ne 20 -or $report.input_to_submit_p95_us_limit -ne 1000 -or
        @($report.output_size).Count -ne 2 -or $report.output_size[0] -ne 1920 -or $report.output_size[1] -ne 1080) { throw 'Latency report top-level contract is invalid.' }
    if (@($report.sources).Count -ne 4) { throw 'Latency report omitted four fixture sources.' }
    for ($i = 0; $i -lt 4; $i++) {
        $source = $report.sources[$i]
        $sourcePath = Normalize-ExtendedPath $source.path
        if ($source.path -isnot [string] -or -not [IO.Path]::IsPathRooted($source.path) -or
            -not [string]::Equals([IO.Path]::GetFullPath($sourcePath), $fixtures[$i], [StringComparison]::OrdinalIgnoreCase) -or
            -not (Test-Integer $source.size_bytes) -or $source.size_bytes -lt 1 -or
            $source.size_bytes -ne (Get-Item -LiteralPath $fixtures[$i]).Length) { throw "Latency report source evidence is invalid at index $i." }
    }
    foreach ($scenarioName in @('one_source', 'four_source')) {
        $scenario = $report.$scenarioName
        $expectedSources = if ($scenarioName -eq 'one_source') { 1 } else { 4 }
        if ($scenario.source_count -ne $expectedSources -or @($scenario.samples).Count -ne 20) { throw "$scenarioName sample count or source count is invalid." }
        $submitSamples = @(); $frameSamples = @()
        for ($i = 0; $i -lt 20; $i++) {
            $sample = $scenario.samples[$i]
            foreach ($name in @('trial', 'sequence_index', 'source_count', 'requested_source_tick', 'input_to_submit_us', 'frame_ready_ms', 'active_sticky_sessions', 'peak_sticky_sessions', 'session_cap', 'post_drop_active_sessions')) { Assert-Unsigned $sample $name "$scenarioName sample $i" }
            $expectedSequenceIndex = if ($scenarioName -eq 'one_source') { if ($i % 2 -eq 0) { 2 * $i } else { 2 * $i + 1 } } else { if ($i % 2 -eq 0) { 2 * $i + 1 } else { 2 * $i } }
            if ($sample.trial -ne $i -or $sample.source_count -ne $expectedSources -or $sample.requested_source_tick -le 1000000 -or
                $sample.sequence_index -ne $expectedSequenceIndex -or
                @($sample.decoded_media_ids).Count -ne $expectedSources -or @($sample.decoded_source_ticks).Count -ne $expectedSources -or
                @($sample.output_size).Count -ne 2 -or $sample.output_size[0] -ne 1920 -or $sample.output_size[1] -ne 1080 -or
                $sample.post_drop_active_sessions -ne 0 -or $sample.session_cap -ne 8) { throw "$scenarioName sample $i violates the isolated Full-output contract." }
            if (@($sample.decoded_media_ids) -join ',' -ne ((1..$expectedSources) -join ',')) { throw "$scenarioName sample $i decoded unexpected media IDs." }
            if (@($sample.decoded_source_ticks | Where-Object { -not (Test-Integer $_) -or $_ -lt $sample.requested_source_tick -or $_ -gt ($sample.requested_source_tick + 33334) }).Count -ne 0 -or
                @($sample.observed_decoder_backends).Count -lt 1 -or
                @($sample.observed_decoder_backends | Where-Object { $_ -isnot [string] -or [string]::IsNullOrWhiteSpace($_) }).Count -ne 0) { throw "$scenarioName sample $i lacks source-tick or backend evidence." }
            $submitSamples += $sample.input_to_submit_us; $frameSamples += $sample.frame_ready_ms
        }
        Assert-Distribution $scenario.input_to_submit_us $submitSamples "$scenarioName input-to-submit"
        Assert-Distribution $scenario.frame_ready_ms $frameSamples "$scenarioName frame-ready"
    }
    $allSequenceIndices = @($report.one_source.samples + $report.four_source.samples | ForEach-Object { $_.sequence_index } | Sort-Object)
    if ($allSequenceIndices.Count -ne 40 -or ($allSequenceIndices -join ',') -ne ((0..39) -join ',')) { throw 'Latency report did not preserve all forty interleaved sequence indices.' }
    foreach ($name in @('input_to_submit_p95_delta_us', 'frame_ready_p95_delta_ms')) { if ($null -eq $report.comparison.PSObject.Properties[$name] -or -not (Test-Integer $report.comparison.$name)) { throw "Latency comparison has invalid $name." } }
    foreach ($name in @('input_to_submit_p95_ratio', 'frame_ready_p95_ratio')) { $value = $report.comparison.$name; if ($null -ne $value -and -not (Test-FiniteNumber $value)) { throw "Latency comparison has non-finite $name." } }
    if ($report.comparison.input_to_submit_p95_delta_us -ne ($report.four_source.input_to_submit_us.p95 - $report.one_source.input_to_submit_us.p95) -or
        $report.comparison.frame_ready_p95_delta_ms -ne ($report.four_source.frame_ready_ms.p95 - $report.one_source.frame_ready_ms.p95)) { throw 'Latency comparison deltas do not match scenario summaries.' }
    foreach ($ratio in @(@('input_to_submit_p95_ratio', $report.four_source.input_to_submit_us.p95, $report.one_source.input_to_submit_us.p95), @('frame_ready_p95_ratio', $report.four_source.frame_ready_ms.p95, $report.one_source.frame_ready_ms.p95))) {
        $actual = $report.comparison.($ratio[0]); $expected = if ($ratio[2] -eq 0) { $null } else { [double]$ratio[1] / [double]$ratio[2] }
        if (($null -eq $actual) -ne ($null -eq $expected) -or ($null -ne $expected -and [Math]::Abs([double]$actual - $expected) -gt 0.0000001)) { throw "Latency comparison $($ratio[0]) does not match scenario summaries." }
    }
    if ($testExitCode -ne 0) { throw "Phase 1 latency comparison test failed; preserved report: $resolvedReportPath" }
    if ($report.status -ne 'passed' -or $report.four_source.input_to_submit_us.p95 -gt $report.input_to_submit_p95_us_limit) { throw "Four-source scheduler p95 exceeded the documented 1 ms headless threshold; preserved report: $resolvedReportPath" }
    Write-Host "Phase 1 latency: PASS ($resolvedReportPath; scheduler p95 $($report.four_source.input_to_submit_us.p95) us; frame-ready p95 $($report.four_source.frame_ready_ms.p95) ms)"
}
finally {
    Restore-EnvironmentValue 'PATH' $savedPath; Restore-EnvironmentValue 'FFMPEG_DIR' $savedFfmpeg; Restore-EnvironmentValue 'LIBCLANG_PATH' $savedLibclang
    Restore-EnvironmentValue 'MAELSTROM_TEST_MEDIA' $savedFirst; Restore-EnvironmentValue 'MAELSTROM_TEST_MEDIA_SECOND' $savedSecond
    Restore-EnvironmentValue 'MAELSTROM_TEST_MEDIA_THIRD' $savedThird; Restore-EnvironmentValue 'MAELSTROM_TEST_MEDIA_FOURTH' $savedFourth
    Restore-EnvironmentValue 'MAELSTROM_PHASE1_LATENCY_REPORT' $savedReport
}
