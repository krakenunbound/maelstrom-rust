#requires -Version 7.0
[CmdletBinding()]
param(
    [ValidateRange(15, 3600)]
    [int]$DurationSeconds = 600,
    [string]$ReportPath
)

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
    if ($null -eq $Value) { return $false }
    try {
        $number = [double]$Value
        return -not [double]::IsNaN($number) -and -not [double]::IsInfinity($number)
    } catch { return $false }
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
    if ($Values.Count -lt 1) { throw "$Context omitted samples." }
    $sorted = @($Values | Sort-Object)
    $p50 = $sorted[[Math]::Ceiling($sorted.Count * .50) - 1]
    $p95 = $sorted[[Math]::Ceiling($sorted.Count * .95) - 1]
    if ($Distribution.p50 -ne $p50 -or $Distribution.p95 -ne $p95 -or $Distribution.max -ne $sorted[-1]) {
        throw "$Context nearest-rank summary does not match raw samples."
    }
}

function Write-AtomicJson([string]$Path, $Value) {
    $temporary = "$Path.$PID.$([Guid]::NewGuid().ToString('N')).tmp"
    try {
        [IO.File]::WriteAllText($temporary, ($Value | ConvertTo-Json -Depth 12), [Text.UTF8Encoding]::new($false))
        if (Test-Path -LiteralPath $Path -PathType Leaf) {
            [IO.File]::Replace($temporary, $Path, $null)
        } else {
            [IO.File]::Move($temporary, $Path)
        }
    } finally {
        Remove-Item -LiteralPath $temporary -Force -ErrorAction SilentlyContinue
    }
}

function Get-TrackedSustainedTestChild([int]$RootProcessId) {
    $processes = @(Get-CimInstance Win32_Process -ErrorAction Stop)
    $descendantIds = [Collections.Generic.HashSet[int]]::new()
    $null = $descendantIds.Add($RootProcessId)
    do {
        $added = $false
        foreach ($candidate in $processes) {
            if ($descendantIds.Contains([int]$candidate.ParentProcessId) -and
                $descendantIds.Add([int]$candidate.ProcessId)) {
                $added = $true
            }
        }
    } while ($added)
    return @($processes | Where-Object {
        $descendantIds.Contains([int]$_.ProcessId) -and
        $_.Name -like 'nle_app-*.exe' -and
        $_.CommandLine -like '*supplied_media_four_video_layers_sustain_bounded_scrub_resources*'
    } | Select-Object -First 1)
}

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$fixtureRunner = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot 'Run-Phase1Multisource.ps1'))
$artifactRoot = [IO.Path]::GetFullPath((Join-Path $repoRoot 'artifacts\phase1-sustained'))
if ([string]::IsNullOrWhiteSpace($ReportPath)) { $ReportPath = Join-Path $artifactRoot 'phase1-sustained-wrapper.json' }
$resolvedReportPath = if ([IO.Path]::IsPathRooted($ReportPath)) { [IO.Path]::GetFullPath($ReportPath) } else { [IO.Path]::GetFullPath((Join-Path $repoRoot $ReportPath)) }
if (-not [string]::Equals([IO.Path]::GetDirectoryName($resolvedReportPath), $artifactRoot, [StringComparison]::OrdinalIgnoreCase) -or
    [IO.Path]::GetExtension($resolvedReportPath) -ine '.json') {
    throw "Report output must be a JSON file directly inside the ignored artifact directory: $artifactRoot"
}

$fixtureRoot = [IO.Path]::GetFullPath((Join-Path $repoRoot 'artifacts\phase1-multisource'))
$fixtures = @(0, 60, 120, 180 | ForEach-Object { [IO.Path]::GetFullPath((Join-Path $fixtureRoot ("source-hue-{0:d3}.mp4" -f $_))) })
$appReportPath = Join-Path $artifactRoot 'phase1-sustained-app-report.json'
$stdoutPath = Join-Path $artifactRoot 'phase1-sustained-test.stdout.txt'
$stderrPath = Join-Path $artifactRoot 'phase1-sustained-test.stderr.txt'
$ffmpegRoot = Join-Path $repoRoot '.deps\ffmpeg-project-8.1'
$libclangRoot = Join-Path $repoRoot '.deps\libclang-bindgen'
$cargoCommand = Get-Command cargo.exe -CommandType Application -ErrorAction Stop
$cargoExecutable = [IO.Path]::GetFullPath($cargoCommand.Source)

$savedPath = $env:PATH; $savedFfmpeg = $env:FFMPEG_DIR; $savedLibclang = $env:LIBCLANG_PATH
$savedFirst = $env:MAELSTROM_TEST_MEDIA; $savedSecond = $env:MAELSTROM_TEST_MEDIA_SECOND
$savedThird = $env:MAELSTROM_TEST_MEDIA_THIRD; $savedFourth = $env:MAELSTROM_TEST_MEDIA_FOURTH
$savedSustainedSeconds = $env:MAELSTROM_PHASE1_SUSTAINED_SECONDS; $savedSustainedReport = $env:MAELSTROM_PHASE1_SUSTAINED_REPORT
$process = $null; $testProcessId = $null; $testExecutable = $null; $failure = $null; $appReport = $null; $memorySamples = @()
$fixtureProvenance = @(); $runLockAcquired = $false; $authoritativeRun = $false
$runLock = [Threading.Mutex]::new($false, 'Local\MaelstromRustPhase1SustainedFixtureLock')
try {
    if (-not $runLock.WaitOne(0)) {
        throw 'Another Phase 1 sustained/fixture run owns the exclusive local artifact lock.'
    }
    $runLockAcquired = $true
} catch {
    if (-not $runLockAcquired) { $runLock.Dispose() }
    throw
}

try {
    New-Item -ItemType Directory -Force -Path $artifactRoot | Out-Null
    # Reuse the fixture contract and its own proof before starting the long-lived test process.
    # It invokes cargo tests only; it never launches the Maelstrom GUI executable.
    & $fixtureRunner
    if ($LASTEXITCODE -ne 0) { throw 'Phase 1 fixture gate failed before the sustained soak.' }
    if (@($fixtures | Where-Object { -not (Test-Path -LiteralPath $_ -PathType Leaf) }).Count -ne 0) {
        throw 'The shared dynamic fixture runner did not produce all four expected sources.'
    }
    $fixtureProvenance = @($fixtures | ForEach-Object {
        $item = Get-Item -LiteralPath $_ -ErrorAction Stop
        [pscustomobject]@{
            path = [IO.Path]::GetFullPath($item.FullName)
            size_bytes = [int64]$item.Length
            sha256 = (Get-FileHash -LiteralPath $item.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
        }
    })
    if ($fixtureProvenance.Count -ne 4 -or @($fixtureProvenance | Where-Object { $_.size_bytes -lt 1 -or -not $_.sha256 }).Count -ne 0) {
        throw 'The shared dynamic fixture runner produced invalid fixture provenance.'
    }

    $env:FFMPEG_DIR = $ffmpegRoot; $env:LIBCLANG_PATH = $libclangRoot
    $env:PATH = (Join-Path $ffmpegRoot 'bin') + [IO.Path]::PathSeparator + $libclangRoot + [IO.Path]::PathSeparator + $savedPath
    $env:MAELSTROM_TEST_MEDIA = $fixtures[0]; $env:MAELSTROM_TEST_MEDIA_SECOND = $fixtures[1]
    $env:MAELSTROM_TEST_MEDIA_THIRD = $fixtures[2]; $env:MAELSTROM_TEST_MEDIA_FOURTH = $fixtures[3]
    $env:MAELSTROM_PHASE1_SUSTAINED_SECONDS = $DurationSeconds.ToString([Globalization.CultureInfo]::InvariantCulture)
    $env:MAELSTROM_PHASE1_SUSTAINED_REPORT = [IO.Path]::GetFullPath($appReportPath)
    Remove-Item -LiteralPath $appReportPath, $resolvedReportPath, $stdoutPath, $stderrPath -Force -ErrorAction SilentlyContinue

    $cargoOutput = & $cargoExecutable test -p nle-app --release --no-run --message-format=json 2>&1
    if ($LASTEXITCODE -ne 0) { throw 'Could not build the release nle-app test executable.' }
    $artifacts = @($cargoOutput | ForEach-Object { try { $_ | ConvertFrom-Json } catch { $null } } | Where-Object { $null -ne $_ })
    $testExecutable = @($artifacts | Where-Object {
        $_.reason -eq 'compiler-artifact' -and $_.target.name -eq 'nle-app' -and $_.target.kind -contains 'bin' -and
        $_.executable -and (Test-Path -LiteralPath $_.executable -PathType Leaf)
    } | Select-Object -Last 1 -ExpandProperty executable)
    if ($testExecutable.Count -ne 1) { throw 'Cargo did not produce exactly one resolved nle-app release test executable.' }
    $testExecutable = [IO.Path]::GetFullPath($testExecutable[0])
    $testExecutableHash = (Get-FileHash -LiteralPath $testExecutable -Algorithm SHA256).Hash.ToLowerInvariant()

    # Cargo is the only process this sustained-test section launches. It owns the test child and
    # gives timeout cleanup one tracked process-tree root without directly starting a raw test executable.
    $process = Start-Process -FilePath $cargoExecutable -ArgumentList @(
        'test', '-p', 'nle-app', '--release',
        'tests::supplied_media_four_video_layers_sustain_bounded_scrub_resources',
        '--', '--ignored', '--exact', '--test-threads=1'
    ) -WindowStyle Hidden -PassThru -RedirectStandardOutput $stdoutPath -RedirectStandardError $stderrPath
    $startedAt = [Diagnostics.Stopwatch]::StartNew()
    $warmBaselineBytes = $null
    $warmAfterSeconds = [Math]::Min(5, [Math]::Max(1, [Math]::Floor($DurationSeconds / 3)))
    $timeoutSeconds = $DurationSeconds + 30
    while (-not $process.HasExited) {
        $process.Refresh()
        $testChild = @(Get-TrackedSustainedTestChild $process.Id)
        if ($testChild.Count -eq 1) {
            $testProcessId = [int]$testChild[0].ProcessId
            $workingSet = [int64]$testChild[0].WorkingSetSize
            $sample = [ordered]@{ elapsed_seconds = [Math]::Round($startedAt.Elapsed.TotalSeconds, 3); working_set_bytes = $workingSet }
            $memorySamples += [pscustomobject]$sample
            if ($null -eq $warmBaselineBytes -and $startedAt.Elapsed.TotalSeconds -ge $warmAfterSeconds) { $warmBaselineBytes = $workingSet }
        }
        if ($startedAt.Elapsed.TotalSeconds -gt $timeoutSeconds) {
            $process.Kill($true)
            $process.WaitForExit()
            throw "Sustained soak test exceeded its $timeoutSeconds-second timeout; stopped only tracked test process tree rooted at PID $($process.Id)."
        }
        Start-Sleep -Seconds 1
    }
    $process.Refresh()
    if ($null -eq $warmBaselineBytes) { throw 'Sustained soak exited before a warm working-set baseline could be sampled.' }
    $testExitCode = $process.ExitCode
    if (-not (Test-Path -LiteralPath $appReportPath -PathType Leaf)) {
        if ($testExitCode -ne 0) { throw "Sustained soak test executable exited with code $testExitCode without an app report." }
        throw 'Sustained soak test did not write its app report.'
    }
    $appReport = Get-Content -LiteralPath $appReportPath -Raw | ConvertFrom-Json
    foreach ($name in @('schema_version', 'requested_duration_seconds', 'source_count', 'cycle_count', 'input_to_submit_p95_us_limit', 'post_drop_active_sessions', 'monitor_dropped_frame_limit')) { Assert-Unsigned $appReport $name 'sustained app report' }
    if (-not (Test-FiniteNumber $appReport.actual_duration_seconds) -or [double]$appReport.actual_duration_seconds -lt $DurationSeconds) {
        throw 'Sustained app report omitted a finite actual duration at least as long as the requested duration.'
    }
    if ($appReport.schema_version -ne 1 -or @('passed', 'failed') -notcontains $appReport.status -or $appReport.requested_duration_seconds -ne $DurationSeconds -or
        $appReport.authoritative -ne ($DurationSeconds -ge 600) -or $appReport.source_count -ne 4 -or $appReport.cycle_count -lt 8 -or
        @($appReport.output_size).Count -ne 2 -or $appReport.output_size[0] -ne 1920 -or $appReport.output_size[1] -ne 1080 -or
        $appReport.input_to_submit_p95_us_limit -ne 1000 -or $appReport.input_to_submit_us.p95 -gt 1000 -or $appReport.post_drop_active_sessions -ne 0) {
        throw 'Sustained app report violates the Full-output bounded scheduler contract.'
    }
    if (@($appReport.source_exercise_counts).Count -ne 4 -or @($appReport.source_exercise_counts | Where-Object { -not (Test-Integer $_) -or $_ -ne $appReport.cycle_count }).Count -ne 0) {
        throw 'Sustained app report did not continuously exercise all four sources.'
    }
    $expectedTicks = @(1117000, 2283000, 3449000, 1617000, 3117000, 1283000, 2617000, 1783000)
    if ((@($appReport.requested_tick_pattern) -join ',') -ne ($expectedTicks -join ',') -or -not (Test-Integer $appReport.max_decoded_tick_delta_us) -or $appReport.max_decoded_tick_delta_us -lt 0 -or $appReport.max_decoded_tick_delta_us -gt 33334) {
        throw 'Sustained app report omitted the exact deterministic forward/backward tick pattern or exceeded the one-frame decoded-tick bound.'
    }
    $sources = @($appReport.sources)
    if ($sources.Count -ne 4) { throw 'Sustained app report omitted four fixture sources.' }
    for ($index = 0; $index -lt 4; $index++) {
        $source = $sources[$index]; $expected = $fixtureProvenance[$index]
        $sourcePath = Normalize-ExtendedPath $source.path
        if ($source.path -isnot [string] -or -not [IO.Path]::IsPathRooted($source.path) -or
            -not [string]::Equals([IO.Path]::GetFullPath($sourcePath), $expected.path, [StringComparison]::OrdinalIgnoreCase) -or
            -not (Test-Integer $source.size_bytes) -or $source.size_bytes -ne $expected.size_bytes) {
            throw "Sustained app report source evidence is invalid at index $index."
        }
    }
    $resources = $appReport.monitor_resources
    foreach ($name in @('frame_cache_capacity_bytes', 'current_frame_cache_bytes', 'peak_frame_cache_bytes_upper_bound', 'active_sticky_sessions', 'peak_sticky_sessions', 'session_cap', 'active_foreground_sessions', 'foreground_session_cap', 'active_background_sessions', 'background_session_cap', 'live_source_groups', 'source_group_cap', 'live_lane_actors', 'lane_actor_cap', 'retiring_lane_actors')) { Assert-Unsigned $resources $name 'sustained monitor resources' }
    if ($resources.current_frame_cache_bytes -gt $resources.frame_cache_capacity_bytes -or $resources.peak_frame_cache_bytes_upper_bound -gt $resources.frame_cache_capacity_bytes -or
        $resources.active_sticky_sessions -ne ($resources.active_foreground_sessions + $resources.active_background_sessions) -or
        $resources.session_cap -ne ($resources.foreground_session_cap + $resources.background_session_cap) -or
        $resources.active_sticky_sessions -gt $resources.session_cap -or $resources.peak_sticky_sessions -gt $resources.session_cap -or
        $resources.active_foreground_sessions -gt $resources.foreground_session_cap -or $resources.active_background_sessions -gt $resources.background_session_cap -or
        $resources.live_source_groups -gt $resources.source_group_cap -or
        ($resources.live_lane_actors + $resources.retiring_lane_actors) -gt $resources.lane_actor_cap) {
        throw 'Sustained app report exceeded bounded cache or session resources.'
    }
    if (@($appReport.observed_decoder_backends).Count -lt 1) { throw 'Sustained app report omitted observed decoder backend evidence.' }
    $diagnostics = $appReport.runtime_diagnostics_delta
    foreach ($name in @('monitor_requests', 'monitor_completed_frames', 'monitor_presented_frames', 'monitor_dropped_frames', 'monitor_hold_events', 'monitor_late_frames', 'monitor_errors', 'native_viewer_uploads', 'fallback_viewer_uploads', 'audio_underrun_frames', 'audio_callback_lock_failures', 'audio_late_discarded_frames')) { Assert-Unsigned $diagnostics $name 'sustained runtime diagnostics delta' }
    $expectedRequests = [int64]$appReport.cycle_count * 4
    $expectedDroppedLimit = [Math]::Max(4, [Math]::Ceiling($expectedRequests / 1000.0))
    if ($appReport.monitor_dropped_frame_limit -ne $expectedDroppedLimit) {
        throw 'Sustained app report omitted the exact bounded stale-event limit.'
    }
    if ($diagnostics.monitor_requests -ne $expectedRequests -or $diagnostics.monitor_completed_frames -lt $expectedRequests -or
        $diagnostics.monitor_presented_frames -ne $diagnostics.monitor_completed_frames -or $diagnostics.monitor_dropped_frames -gt $expectedDroppedLimit -or
        $diagnostics.monitor_hold_events -gt $expectedRequests -or $diagnostics.monitor_late_frames -gt $expectedRequests -or
        $diagnostics.monitor_errors -ne 0 -or ($diagnostics.native_viewer_uploads + $diagnostics.fallback_viewer_uploads) -ne $diagnostics.monitor_presented_frames -or
        $diagnostics.audio_underrun_frames -ne 0 -or $diagnostics.audio_callback_lock_failures -ne 0 -or $diagnostics.audio_late_discarded_frames -ne 0) {
        throw 'Sustained app report violated the monitored workload counter bounds.'
    }
    Assert-Distribution $appReport.input_to_submit_us @($appReport.input_to_submit_samples_us) 'sustained input-to-submit summary'
    Assert-Distribution $appReport.frame_ready_ms @($appReport.frame_ready_samples_ms) 'sustained frame-ready summary'

    if ($testExitCode -ne 0 -or $appReport.status -ne 'passed') {
        throw "Sustained soak test reported status '$($appReport.status)' with exit code $testExitCode; app report preserved."
    }

    $authoritativeRun = $DurationSeconds -ge 600 -and $appReport.authoritative -eq $true -and [double]$appReport.actual_duration_seconds -ge $DurationSeconds

    $peakWorkingSetBytes = [int64](($memorySamples | Measure-Object -Property working_set_bytes -Maximum).Maximum)
    $growthBytes = $peakWorkingSetBytes - $warmBaselineBytes
    # Bounded decoded-frame cache capacity plus 512 MiB allocator/codec headroom. This is
    # intentionally generous and diagnostic: it is not a cross-machine memory certification.
    $growthBoundBytes = [int64]$resources.frame_cache_capacity_bytes + 512MB
    if ($growthBytes -gt $growthBoundBytes) {
        throw "Sustained soak working-set growth exceeded the cache-plus-headroom $growthBoundBytes-byte bound: $growthBytes bytes."
    }
}
catch {
    $failure = $_.Exception.Message
}
finally {
    try {
        if ($null -ne $process -and -not $process.HasExited) {
            try { $process.Kill($true); $process.WaitForExit() } catch { }
        }
        $appReportHash = if (Test-Path -LiteralPath $appReportPath -PathType Leaf) { (Get-FileHash -LiteralPath $appReportPath -Algorithm SHA256).Hash.ToLowerInvariant() } else { $null }
        $wrapper = [ordered]@{
            schema_version = 1
            status = if ($null -eq $failure) { 'passed' } else { 'failed' }
            failure = $failure
            requested_duration_seconds = $DurationSeconds
            requested_authoritative_duration = ($DurationSeconds -ge 600)
            authoritative = ($null -eq $failure -and $authoritativeRun)
            actual_duration_seconds = if ($null -ne $appReport -and (Test-FiniteNumber $appReport.actual_duration_seconds)) { [double]$appReport.actual_duration_seconds } else { $null }
            fixture_provenance = $fixtureProvenance
            test_executable_path = $testExecutable
            test_executable_sha256 = if ($null -ne $testExecutable -and (Test-Path -LiteralPath $testExecutable -PathType Leaf)) { (Get-FileHash -LiteralPath $testExecutable -Algorithm SHA256).Hash.ToLowerInvariant() } else { $null }
            tracked_pid = $testProcessId
            tracked_cargo_pid = if ($null -ne $process) { $process.Id } else { $null }
            memory_samples = $memorySamples
            warm_baseline_working_set_bytes = $warmBaselineBytes
            peak_working_set_bytes = if ($memorySamples.Count) { [int64](($memorySamples | Measure-Object -Property working_set_bytes -Maximum).Maximum) } else { $null }
            working_set_growth_bytes = if ($null -ne $warmBaselineBytes -and $memorySamples.Count) { [int64](($memorySamples | Measure-Object -Property working_set_bytes -Maximum).Maximum) - $warmBaselineBytes } else { $null }
            working_set_growth_bound_bytes = if ($null -ne $appReport) { [int64]$appReport.monitor_resources.frame_cache_capacity_bytes + 512MB } else { $null }
            app_report_path = if (Test-Path -LiteralPath $appReportPath -PathType Leaf) { [IO.Path]::GetFullPath($appReportPath) } else { $null }
            app_report_sha256 = $appReportHash
            stdout_path = if (Test-Path -LiteralPath $stdoutPath -PathType Leaf) { [IO.Path]::GetFullPath($stdoutPath) } else { $null }
            stderr_path = if (Test-Path -LiteralPath $stderrPath -PathType Leaf) { [IO.Path]::GetFullPath($stderrPath) } else { $null }
        }
        New-Item -ItemType Directory -Force -Path $artifactRoot | Out-Null
        Write-AtomicJson $resolvedReportPath $wrapper
    }
    finally {
        Restore-EnvironmentValue 'PATH' $savedPath; Restore-EnvironmentValue 'FFMPEG_DIR' $savedFfmpeg; Restore-EnvironmentValue 'LIBCLANG_PATH' $savedLibclang
        Restore-EnvironmentValue 'MAELSTROM_TEST_MEDIA' $savedFirst; Restore-EnvironmentValue 'MAELSTROM_TEST_MEDIA_SECOND' $savedSecond; Restore-EnvironmentValue 'MAELSTROM_TEST_MEDIA_THIRD' $savedThird; Restore-EnvironmentValue 'MAELSTROM_TEST_MEDIA_FOURTH' $savedFourth
        Restore-EnvironmentValue 'MAELSTROM_PHASE1_SUSTAINED_SECONDS' $savedSustainedSeconds; Restore-EnvironmentValue 'MAELSTROM_PHASE1_SUSTAINED_REPORT' $savedSustainedReport
        if ($runLockAcquired) { $runLock.ReleaseMutex(); $runLockAcquired = $false }
        $runLock.Dispose()
    }
}
if ($null -ne $failure) { throw "Phase 1 sustained soak failed; preserved evidence: $resolvedReportPath. $failure" }
Write-Host "Phase 1 sustained soak: PASS ($resolvedReportPath; duration $DurationSeconds s; executable $testExecutable)"
