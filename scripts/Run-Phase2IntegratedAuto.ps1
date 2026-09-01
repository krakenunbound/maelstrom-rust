[CmdletBinding()]
param([string]$ReportPath)

$ErrorActionPreference = 'Stop'

function Restore-EnvironmentValue([string]$Name, $Value) {
    if ($null -eq $Value) { Remove-Item "Env:$Name" -ErrorAction SilentlyContinue }
    else { Set-Item "Env:$Name" $Value }
}

function Get-ReportProperty($Object, [string]$Name, [string]$Context) {
    $property = $Object.PSObject.Properties[$Name]
    if ($null -eq $property -or $null -eq $property.Value) { throw "$Context is missing required property '$Name'." }
    return $property.Value
}

function Test-JsonInteger($Value) {
    return $Value -is [byte] -or $Value -is [sbyte] -or $Value -is [int16] -or $Value -is [uint16] -or
        $Value -is [int32] -or $Value -is [uint32] -or $Value -is [int64] -or $Value -is [uint64]
}

function Require-JsonInteger($Value, [string]$Context, [int64]$Minimum = 0) {
    if (-not (Test-JsonInteger $Value) -or [int64]$Value -lt $Minimum) { throw "$Context must be an integer no less than $Minimum." }
}

function Require-Bool($Value, [string]$Context, [bool]$Expected) {
    if ($Value -isnot [bool] -or $Value -ne $Expected) { throw "$Context must be boolean $Expected." }
}

function Require-NonemptyString($Value, [string]$Context) {
    if ($Value -isnot [string] -or [string]::IsNullOrWhiteSpace($Value)) { throw "$Context must be a nonempty string." }
}

function Normalize-Path([string]$Path) {
    if ($Path.StartsWith('\\?\')) { $Path = $Path.Substring(4) }
    return [IO.Path]::GetFullPath($Path)
}

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$cargo = 'C:\Users\The Kraken\.cargo\bin\cargo.exe'
$ffmpegRoot = Join-Path $repoRoot '.deps\ffmpeg-project-8.1'
$libclangRoot = Join-Path $repoRoot '.deps\libclang-bindgen'
$fixtureRoot = Join-Path $repoRoot 'artifacts\phase1-multisource'
$artifactRoot = [IO.Path]::GetFullPath((Join-Path $repoRoot 'artifacts\phase2-integrated-auto'))
if ([string]::IsNullOrWhiteSpace($ReportPath)) { $ReportPath = Join-Path $artifactRoot 'phase2-integrated-auto.schema1.json' }
$resolvedReportPath = if ([IO.Path]::IsPathRooted($ReportPath)) { [IO.Path]::GetFullPath($ReportPath) } else { [IO.Path]::GetFullPath((Join-Path $repoRoot $ReportPath)) }
if (-not [string]::Equals([IO.Path]::GetDirectoryName($resolvedReportPath), $artifactRoot, [StringComparison]::OrdinalIgnoreCase) -or [IO.Path]::GetExtension($resolvedReportPath) -ine '.json') {
    throw "Report output must be a JSON file directly inside the ignored artifact directory: $artifactRoot"
}
foreach ($path in @($cargo, (Join-Path $ffmpegRoot 'bin\ffmpeg.exe'), (Join-Path $ffmpegRoot 'bin\ffprobe.exe'), (Join-Path $libclangRoot 'libclang.dll'))) {
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "Missing required local qualification dependency: $path" }
}

$saved = @{}
foreach ($name in @('PATH','FFMPEG_DIR','LIBCLANG_PATH','MAELSTROM_TEST_MEDIA','MAELSTROM_TEST_MEDIA_SECOND','MAELSTROM_TEST_MEDIA_THIRD','MAELSTROM_TEST_MEDIA_FOURTH','MAELSTROM_PHASE2_INTEGRATED_AUTO_REPORT')) {
    $saved[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
}
$fixtureMutex = [Threading.Mutex]::new($false, 'Local\MaelstromRustPhase1SustainedFixtureLock')
$fixtureMutexHeld = $false
try {
    New-Item -ItemType Directory -Force -Path $artifactRoot | Out-Null
    # The existing Phase 1 gate is the authoritative dynamic-fixture generator and validator.
    & (Join-Path $PSScriptRoot 'Run-Phase1Multisource.ps1') -ReportPath (Join-Path $fixtureRoot 'phase1-multisource.json')
    if ($LASTEXITCODE -ne 0) { throw 'Phase 1 fixture generator/gate failed.' }
    $fixtures = @(0,60,120,180 | ForEach-Object { Join-Path $fixtureRoot ('source-hue-{0:d3}.mp4' -f $_) })
    foreach ($fixture in $fixtures) { if (-not (Test-Path -LiteralPath $fixture -PathType Leaf)) { throw "Missing generated Phase 1 fixture: $fixture" } }
    if (-not $fixtureMutex.WaitOne(0)) { throw 'Another Phase 1 fixture consumer owns the exclusive local artifact lock.' }
    $fixtureMutexHeld = $true
    $env:PATH = (Join-Path $ffmpegRoot 'bin') + [IO.Path]::PathSeparator + $libclangRoot + [IO.Path]::PathSeparator + $saved['PATH']
    $env:FFMPEG_DIR = $ffmpegRoot; $env:LIBCLANG_PATH = $libclangRoot
    $env:MAELSTROM_TEST_MEDIA = [IO.Path]::GetFullPath($fixtures[0]); $env:MAELSTROM_TEST_MEDIA_SECOND = [IO.Path]::GetFullPath($fixtures[1])
    $env:MAELSTROM_TEST_MEDIA_THIRD = [IO.Path]::GetFullPath($fixtures[2]); $env:MAELSTROM_TEST_MEDIA_FOURTH = [IO.Path]::GetFullPath($fixtures[3])
    $env:MAELSTROM_PHASE2_INTEGRATED_AUTO_REPORT = $resolvedReportPath
    Remove-Item -LiteralPath $resolvedReportPath -Force -ErrorAction SilentlyContinue
    Push-Location -LiteralPath $repoRoot
    try {
        & $cargo test -p nle-app --release tests::supplied_media_two_layers_auto_quality_downshifts_and_composites_on_integrated_gpu -- --ignored --exact --test-threads=1
        if ($LASTEXITCODE -ne 0) { throw 'Phase 2 integrated Auto test failed.' }
    } finally { Pop-Location }
    if (-not (Test-Path -LiteralPath $resolvedReportPath -PathType Leaf)) { throw 'Phase 2 test passed without writing its report.' }
    $report = Get-Content -LiteralPath $resolvedReportPath -Raw | ConvertFrom-Json
    $schemaVersion = Get-ReportProperty $report 'schema_version' 'Phase 2 report'
    Require-JsonInteger $schemaVersion 'Phase 2 schema_version' 1
    if ($schemaVersion -ne 1 -or
        (Get-ReportProperty $report 'status' 'Phase 2 report') -cne 'passed' -or
        (Get-ReportProperty $report 'scope' 'Phase 2 report') -cne 'headless_app_auto_scheduler_and_integrated_compositor' -or
        (Get-ReportProperty $report 'timing_pressure_note' 'Phase 2 report') -cne 'Four one-second synthetic turnaround samples were injected immediately before production apply_monitor_decode_event; this proves Auto hysteresis, not organic decode latency.') {
        throw 'Phase 2 report schema/scope/timing-pressure contract is invalid.'
    }
    Require-Bool (Get-ReportProperty $report 'app_auto_preview_observed' 'Phase 2 report') 'Phase 2 app_auto_preview_observed' $true
    Require-Bool (Get-ReportProperty $report 'integrated_gpu_compositor_observed' 'Phase 2 report') 'Phase 2 integrated_gpu_compositor_observed' $true
    Require-Bool (Get-ReportProperty $report 'window_surface_observed' 'Phase 2 report') 'Phase 2 window_surface_observed' $false
    Require-Bool (Get-ReportProperty $report 'physical_scanout_observed' 'Phase 2 report') 'Phase 2 physical_scanout_observed' $false
    Require-Bool (Get-ReportProperty $report 'controlled_timing_pressure_injected' 'Phase 2 report') 'Phase 2 controlled_timing_pressure_injected' $true
    $adapter = Get-ReportProperty $report 'adapter' 'Phase 2 report'
    if ((Get-ReportProperty $adapter 'device_type' 'Phase 2 adapter') -cne 'IntegratedGpu' -or (Get-ReportProperty $adapter 'backend' 'Phase 2 adapter') -cne 'Dx12') { throw 'Phase 2 report adapter is not exact DX12 IntegratedGpu.' }
    Require-NonemptyString (Get-ReportProperty $adapter 'name' 'Phase 2 adapter') 'Phase 2 adapter name'
    foreach ($name in @('driver','driver_info')) {
        $value = Get-ReportProperty $adapter $name 'Phase 2 adapter'
        if ($value -isnot [string]) { throw "Phase 2 adapter $name must be a string." }
    }
    foreach ($name in @('vendor','device')) { Require-JsonInteger (Get-ReportProperty $adapter $name 'Phase 2 adapter') "Phase 2 adapter $name" 1 }
    $reportSources = @(Get-ReportProperty $report 'sources' 'Phase 2 report')
    if ($reportSources.Count -ne 2) { throw 'Phase 2 report must contain exactly two sources.' }
    for ($index = 0; $index -lt 2; $index++) {
        $source = $reportSources[$index]; $expectedFixture = [IO.Path]::GetFullPath($fixtures[$index])
        $reportedPath = Get-ReportProperty $source 'path' "Phase 2 source $index"
        Require-NonemptyString $reportedPath "Phase 2 source $index path"
        if ((Normalize-Path $reportedPath) -cne $expectedFixture) { throw "Phase 2 source $index does not match its generated hue fixture." }
        $reportedBytes = Get-ReportProperty $source 'size_bytes' "Phase 2 source $index"
        Require-JsonInteger $reportedBytes "Phase 2 source $index size_bytes" 1
        if ([int64]$reportedBytes -ne (Get-Item -LiteralPath $expectedFixture).Length) { throw "Phase 2 source $index file size changed." }
        $probe = & (Join-Path $ffmpegRoot 'bin\ffprobe.exe') -v error -select_streams v:0 -show_entries stream=width,height -of default=noprint_wrappers=1 $expectedFixture
        if ($LASTEXITCODE -ne 0 -or $probe -notcontains 'width=1920' -or $probe -notcontains 'height=1080') { throw "Phase 2 source $index is not the required 1920x1080 fixture." }
    }
    $initialRequests = @(Get-ReportProperty $report 'initial_requests' 'Phase 2 report'); $halfRequests = @(Get-ReportProperty $report 'half_requests' 'Phase 2 report')
    if ($initialRequests.Count -ne 2 -or $halfRequests.Count -ne 2) { throw 'Phase 2 report must contain exactly two initial and Half requests.' }
    $probes = @(Get-ReportProperty $report 'probes' 'Phase 2 report')
    if ($probes.Count -ne 2) { throw 'Phase 2 report must contain exactly two compositor probes.' }
    $decoderBackends = @(Get-ReportProperty $report 'decoded_backends' 'Phase 2 report')
    if ($decoderBackends.Count -lt 1) { throw 'Phase 2 report omitted decoder backend evidence.' }
    foreach ($backend in $decoderBackends) { Require-NonemptyString $backend 'Phase 2 decoder backend' }
    $uploadSerials = @(Get-ReportProperty $report 'upload_serials' 'Phase 2 report'); $composedSerials = @(Get-ReportProperty $report 'composed_upload_serials' 'Phase 2 report')
    if ($uploadSerials.Count -ne 4 -or $composedSerials.Count -ne 4) { throw 'Phase 2 report compositor serial arrays must have four slots.' }
    for ($layer = 0; $layer -lt 2; $layer++) {
        $initial = $initialRequests[$layer]; $half = $halfRequests[$layer]
        foreach ($request in @($initial, $half)) {
            foreach ($name in @('generation','media_id','request_id')) { Require-JsonInteger (Get-ReportProperty $request $name "Phase 2 layer $layer request") "Phase 2 layer $layer $name" 1 }
            Require-JsonInteger (Get-ReportProperty $request 'source_tick' "Phase 2 layer $layer request") "Phase 2 layer $layer source_tick" 0
            $dimensions = @(Get-ReportProperty $request 'dimensions' "Phase 2 layer $layer request")
            if ($dimensions.Count -ne 2) { throw "Phase 2 layer $layer request dimensions are invalid." }
            foreach ($dimension in $dimensions) { Require-JsonInteger $dimension "Phase 2 layer $layer request dimension" 1 }
        }
        if ((Get-ReportProperty $initial 'media_id' "Phase 2 initial layer $layer") -ne $layer + 1 -or (Get-ReportProperty $half 'media_id' "Phase 2 Half layer $layer") -ne $layer + 1 -or
            (Get-ReportProperty $initial 'selected_quality' "Phase 2 initial layer $layer") -cne 'Auto' -or (Get-ReportProperty $initial 'resolved_quality' "Phase 2 initial layer $layer") -cne 'Full' -or
            (Get-ReportProperty $half 'selected_quality' "Phase 2 Half layer $layer") -cne 'Auto' -or (Get-ReportProperty $half 'resolved_quality' "Phase 2 Half layer $layer") -cne 'Half' -or
            $initial.dimensions[0] -ne 640 -or $initial.dimensions[1] -ne 360 -or
            $half.dimensions[0] -ne 320 -or $half.dimensions[1] -ne 180 -or
            $half.dimensions[0] * 2 -ne $initial.dimensions[0] -or $half.dimensions[1] * 2 -ne $initial.dimensions[1] -or
            $half.generation -le $initial.generation -or $half.request_id -le $initial.request_id) {
            throw "Phase 2 report failed layer $layer Auto resubmission/compositor evidence."
        }
        Require-JsonInteger $uploadSerials[$layer] "Phase 2 upload serial $layer" 1
        Require-JsonInteger $composedSerials[$layer] "Phase 2 composed serial $layer" 1
        if ($composedSerials[$layer] -ne $uploadSerials[$layer]) { throw "Phase 2 layer $layer composition did not use its current upload." }
        $probe = $probes[$layer]
        $probeLayer = Get-ReportProperty $probe 'layer' "Phase 2 probe $layer"
        $probeTolerance = Get-ReportProperty $probe 'tolerance' "Phase 2 probe $layer"
        Require-JsonInteger $probeLayer "Phase 2 probe $layer layer" 0
        Require-JsonInteger $probeTolerance "Phase 2 probe $layer tolerance" 0
        if ($probeLayer -ne $layer -or $probe.passed -isnot [bool] -or -not $probe.passed -or $probeTolerance -ne 24 -or
            @($probe.coordinate).Count -ne 2 -or @($probe.actual_rgba).Count -ne 4 -or @($probe.expected_rgba).Count -ne 4) {
            throw "Phase 2 report failed layer $layer probe schema/evidence."
        }
        foreach ($coordinate in @($probe.coordinate)) { Require-JsonInteger $coordinate "Phase 2 layer $layer probe coordinate" 0 }
        $expectedX = if ($layer -eq 0) { [int]($half.dimensions[0] / 4) } else { [int]($half.dimensions[0] * 3 / 4) }
        $expectedY = [int]($half.dimensions[1] / 2)
        if ($probe.coordinate[0] -ne $expectedX -or $probe.coordinate[1] -ne $expectedY) {
            throw "Phase 2 report used an unexpected layer $layer probe coordinate."
        }
        for ($channel = 0; $channel -lt 4; $channel++) {
            Require-JsonInteger $probe.actual_rgba[$channel] "Phase 2 layer $layer actual RGBA $channel" 0
            Require-JsonInteger $probe.expected_rgba[$channel] "Phase 2 layer $layer expected RGBA $channel" 0
            if ($probe.actual_rgba[$channel] -gt 255 -or $probe.expected_rgba[$channel] -gt 255) { throw "Phase 2 layer $layer probe channel is outside 0..255." }
            if ([Math]::Abs([int]$probe.actual_rgba[$channel] - [int]$probe.expected_rgba[$channel]) -gt [int]$probe.tolerance) {
                throw "Phase 2 report failed layer $layer probe tolerance on RGBA channel $channel."
            }
        }
    }
    for ($layer = 2; $layer -lt 4; $layer++) {
        Require-JsonInteger $uploadSerials[$layer] "Phase 2 unused upload serial $layer" 0
        if ($uploadSerials[$layer] -ne 0 -or $null -ne $composedSerials[$layer]) { throw "Phase 2 unused compositor layer $layer retained evidence." }
    }
    Write-Host "Phase 2 integrated Auto: PASS ($resolvedReportPath)"
} finally {
    foreach ($name in $saved.Keys) { Restore-EnvironmentValue $name $saved[$name] }
    if ($fixtureMutexHeld) { $fixtureMutex.ReleaseMutex() }
    $fixtureMutex.Dispose()
}
