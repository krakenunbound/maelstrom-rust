#requires -Version 7.0
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateNotNullOrEmpty()]
    [string]$FfmpegRoot,
    [ValidateRange(15, 3600)]
    [int]$DurationSeconds = 600,
    [string]$ReportPath
)

$ErrorActionPreference = 'Stop'

function Test-AbsolutePath([string]$Path) {
    [IO.Path]::IsPathRooted($Path) -and [string]::Equals([IO.Path]::GetFullPath($Path), $Path, [StringComparison]::OrdinalIgnoreCase)
}

function Enter-Phase0ScenarioMutex {
    $mutex = [Threading.Mutex]::new($false, 'Local\MaelstromPhase0ScenarioMatrix')
    try {
        try { $acquired = $mutex.WaitOne(0) }
        catch [Threading.AbandonedMutexException] { $acquired = $true }
        if (-not $acquired) { throw 'Another Phase 0 scenario matrix is already running.' }
        return $mutex
    } catch {
        $mutex.Dispose()
        throw
    }
}

function Write-AtomicJson([string]$Path, $Value) {
    $temporary = "$Path.$PID.$([Guid]::NewGuid().ToString('N')).tmp"
    $backup = "$Path.$PID.$([Guid]::NewGuid().ToString('N')).bak"
    try {
        [IO.File]::WriteAllText($temporary, ($Value | ConvertTo-Json -Depth 16), [Text.UTF8Encoding]::new($false))
        if (Test-Path -LiteralPath $Path -PathType Leaf) { [IO.File]::Replace($temporary, $Path, $backup) }
        else { [IO.File]::Move($temporary, $Path) }
    } finally {
        Remove-Item -LiteralPath $temporary -Force -ErrorAction SilentlyContinue
        Remove-Item -LiteralPath $backup -Force -ErrorAction SilentlyContinue
    }
}

function Read-ScenarioReport([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { throw "Phase 0 child run did not write its report: $Path" }
    $report = Get-Content -LiteralPath $Path -Raw | ConvertFrom-Json
    if ($report.schema_version -ne 3 -or @('passed', 'failed') -notcontains $report.status -or
        [int]$report.scenario_count -ne 6 -or @($report.scenarios).Count -ne 6) {
        throw "Phase 0 child report has an unexpected schema, status, or scenario count: $Path"
    }
    foreach ($scenario in @($report.scenarios)) {
        if ([string]::IsNullOrWhiteSpace($scenario.name) -or [int]$scenario.iterations -lt 1 -or
            [double]$scenario.elapsed_ms -lt 0 -or -not ($scenario.passed -is [bool]) -or
            [string]::IsNullOrWhiteSpace($scenario.evidence)) {
            throw "Invalid Phase 0 scenario evidence in child report: $($scenario.name)"
        }
    }
    return $report
}

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$runner = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot 'Run-Phase0Scenarios.ps1'))
$artifactRoot = [IO.Path]::GetFullPath((Join-Path $repoRoot 'artifacts\phase0-sustained-scenarios'))
if ([string]::IsNullOrWhiteSpace($ReportPath)) { $ReportPath = Join-Path $artifactRoot 'phase0-sustained-scenarios.json' }
$resolvedReportPath = if ([IO.Path]::IsPathRooted($ReportPath)) { [IO.Path]::GetFullPath($ReportPath) } else { [IO.Path]::GetFullPath((Join-Path $repoRoot $ReportPath)) }
if (-not [string]::Equals([IO.Path]::GetDirectoryName($resolvedReportPath), $artifactRoot, [StringComparison]::OrdinalIgnoreCase) -or
    [IO.Path]::GetExtension($resolvedReportPath) -ine '.json') {
    throw "Report output must be a JSON file directly inside the ignored artifact directory: $artifactRoot"
}

$orchestratorStartedUtc = [DateTime]::UtcNow
$setupStopwatch = [Diagnostics.Stopwatch]::StartNew()
$matrixStartedUtc = $null
$matrixStopwatch = [Diagnostics.Stopwatch]::new()
$failure = $null
$runs = @()
$scenarioTotals = @{}
$childReportPath = $null
$ffmpegRootPath = $null
$ffmpegVersion = $null
$phase0Mutex = $null
$machineEvidence = [ordered]@{
    computer_name = $env:COMPUTERNAME
    os = [Environment]::OSVersion.VersionString
    process_architecture = [Runtime.InteropServices.RuntimeInformation]::ProcessArchitecture.ToString()
    processor_count = [Environment]::ProcessorCount
    cpu_model = $null
    physical_memory_bytes = $null
    graphics_adapters = @()
    powershell_version = $PSVersionTable.PSVersion.ToString()
}
try {
    New-Item -ItemType Directory -Force -Path $artifactRoot | Out-Null
    if (-not (Test-AbsolutePath $FfmpegRoot)) { throw 'FFmpeg root must be an absolute path.' }
    $ffmpegRootPath = (Resolve-Path -LiteralPath $FfmpegRoot).Path
    $ffmpeg = Join-Path $ffmpegRootPath 'bin\ffmpeg.exe'
    if (-not (Test-Path -LiteralPath $ffmpeg -PathType Leaf)) { throw "Expected ffmpeg.exe below $ffmpegRootPath\\bin." }
    $ffmpegVersion = ((& $ffmpeg -hide_banner -version 2>&1) | Select-Object -First 1)
    if ($LASTEXITCODE -ne 0 -or $ffmpegVersion -notmatch '^ffmpeg version n?8\.1(?:[.\s-]|$)') {
        throw "Phase 0 sustained scenarios require the pinned FFmpeg 8.1 bundle: $ffmpegRootPath"
    }
    try {
        $processor = Get-CimInstance Win32_Processor -ErrorAction Stop | Select-Object -First 1
        $computer = Get-CimInstance Win32_ComputerSystem -ErrorAction Stop | Select-Object -First 1
        $adapters = @(Get-CimInstance Win32_VideoController -ErrorAction Stop | ForEach-Object {
            [ordered]@{
                name = [string]$_.Name
                driver_version = [string]$_.DriverVersion
                adapter_ram_bytes = if ($null -ne $_.AdapterRAM) { [int64]$_.AdapterRAM } else { $null }
                current_refresh_hz = if ($null -ne $_.CurrentRefreshRate) { [int64]$_.CurrentRefreshRate } else { $null }
            }
        })
        $machineEvidence.cpu_model = [string]$processor.Name
        $machineEvidence.physical_memory_bytes = [int64]$computer.TotalPhysicalMemory
        $machineEvidence.graphics_adapters = $adapters
    } catch {
        # Basic runtime identity remains useful if WMI/CIM is unavailable.
    }

    $phase0Mutex = Enter-Phase0ScenarioMutex
    $childArtifactRoot = [IO.Path]::GetFullPath((Join-Path $repoRoot 'artifacts\phase0-scenarios'))
    $setupStopwatch.Stop()
    $matrixStartedUtc = [DateTime]::UtcNow
    $matrixStopwatch.Start()
    $runIndex = 0
    do {
        $runIndex++
        $childReportPath = Join-Path $childArtifactRoot ("phase0-sustained-child-{0}-{1}-{2}.json" -f $PID, $runIndex, [Guid]::NewGuid().ToString('N'))
        $runStartedUtc = [DateTime]::UtcNow
        $runStopwatch = [Diagnostics.Stopwatch]::StartNew()
        try {
            $arguments = @{ FfmpegRoot = $ffmpegRootPath; ReportPath = $childReportPath }
            if ($runIndex -gt 1) { $arguments.SkipFixtureValidation = $true }
            # The child report is authoritative. Suppress repeated Cargo/compiler output so a
            # 600-second run does not accumulate an unbounded console log.
            $childInvocationFailure = $null
            $childExitCode = $null
            try {
                & $runner @arguments *> $null
                $childExitCode = $LASTEXITCODE
            } catch {
                $childInvocationFailure = $_.Exception.Message
                $childExitCode = $LASTEXITCODE
            }

            $child = $null
            $childReadFailure = $null
            if (Test-Path -LiteralPath $childReportPath -PathType Leaf) {
                try { $child = Read-ScenarioReport $childReportPath }
                catch { $childReadFailure = $_.Exception.Message }
            } else {
                $childReadFailure = 'Child report was not written.'
            }
            $runScenarios = if ($null -ne $child) {
                @($child.scenarios | ForEach-Object {
                    [ordered]@{
                        name = [string]$_.name
                        iterations = [int64]$_.iterations
                        elapsed_ms = [double]$_.elapsed_ms
                        passed = [bool]$_.passed
                        evidence = [string]$_.evidence
                        decoder_backend = $_.decoder_backend
                        encoder_backend = $_.encoder_backend
                    }
                })
            } else { @() }
            foreach ($scenario in $runScenarios) {
                if (-not $scenarioTotals.ContainsKey($scenario.name)) {
                    $scenarioTotals[$scenario.name] = [ordered]@{
                        name = $scenario.name
                        run_count = 0
                        passed_run_count = 0
                        iterations = [int64]0
                        elapsed_ms = [double]0
                    }
                }
                $total = $scenarioTotals[$scenario.name]
                $total.run_count++; if ($scenario.passed) { $total.passed_run_count++ }
                $total.iterations += [int64]$scenario.iterations; $total.elapsed_ms += [double]$scenario.elapsed_ms
            }
            $childHash = if (Test-Path -LiteralPath $childReportPath -PathType Leaf) {
                (Get-FileHash -LiteralPath $childReportPath -Algorithm SHA256).Hash.ToLowerInvariant()
            } else { $null }
            $runs += [pscustomobject][ordered]@{
                run_index = $runIndex; started_utc = $runStartedUtc.ToString('O'); ended_utc = [DateTime]::UtcNow.ToString('O')
                elapsed_seconds = [Math]::Round($runStopwatch.Elapsed.TotalSeconds, 3); child_exit_code = $childExitCode
                status = if ($null -ne $child) { [string]$child.status } else { 'unreported' }
                invocation_failure = $childInvocationFailure; report_read_failure = $childReadFailure
                report_sha256 = $childHash; scenario_count = $runScenarios.Count; scenarios = $runScenarios
            }
            if ($null -ne $childInvocationFailure -or $null -ne $childReadFailure -or
                $null -eq $child -or $childExitCode -ne 0 -or $child.status -ne 'passed' -or
                @($runScenarios | Where-Object { -not $_.passed }).Count -ne 0) {
                throw "Phase 0 child run $runIndex failed; its available report evidence is embedded in the sustained report. invocation=$childInvocationFailure read=$childReadFailure exit=$childExitCode"
            }
        } finally {
            if ($null -ne $childReportPath) { Remove-Item -LiteralPath $childReportPath -Force -ErrorAction SilentlyContinue }
            $childReportPath = $null
        }
    } while ($matrixStopwatch.Elapsed.TotalSeconds -lt $DurationSeconds)
} catch {
    $failure = $_.Exception.Message
} finally {
    if ($setupStopwatch.IsRunning) { $setupStopwatch.Stop() }
    if ($matrixStopwatch.IsRunning) { $matrixStopwatch.Stop() }
    $endedUtc = [DateTime]::UtcNow
    $totals = @($scenarioTotals.Values | Sort-Object name | ForEach-Object {
        [ordered]@{ name = $_.name; run_count = [int64]$_.run_count; passed_run_count = [int64]$_.passed_run_count; iterations = [int64]$_.iterations; elapsed_ms = [Math]::Round([double]$_.elapsed_ms, 3) }
    })
    if ($null -eq $failure -and ($runs.Count -lt 1 -or $totals.Count -ne 6 -or $matrixStopwatch.Elapsed.TotalSeconds -lt $DurationSeconds)) {
        $failure = "Sustained matrix ended without the required duration, run, or six-scenario evidence."
    }
    $passed = $null -eq $failure
    $decoderBackends = @($runs | ForEach-Object { $_.scenarios } | ForEach-Object {
        $_.decoder_backend
    } | Where-Object { -not [string]::IsNullOrWhiteSpace([string]$_) } | Sort-Object -Unique)
    $encoderBackends = @($runs | ForEach-Object { $_.scenarios } | ForEach-Object {
        $_.encoder_backend
    } | Where-Object { -not [string]::IsNullOrWhiteSpace([string]$_) } | Sort-Object -Unique)
    $totalScenarioRuns = [int64](($runs | Measure-Object -Property scenario_count -Sum).Sum)
    $wrapper = [ordered]@{
        schema_version = 1; status = if ($passed) { 'passed' } else { 'failed' }; failure = $failure
        requested_duration_seconds = $DurationSeconds; authoritative_duration_requested = ($DurationSeconds -ge 600)
        authoritative = ($passed -and $DurationSeconds -ge 600 -and $matrixStopwatch.Elapsed.TotalSeconds -ge $DurationSeconds)
        orchestrator_started_utc = $orchestratorStartedUtc.ToString('O')
        matrix_started_utc = if ($null -ne $matrixStartedUtc) { $matrixStartedUtc.ToString('O') } else { $null }
        ended_utc = $endedUtc.ToString('O')
        setup_duration_seconds = [Math]::Round($setupStopwatch.Elapsed.TotalSeconds, 3)
        matrix_duration_seconds = [Math]::Round($matrixStopwatch.Elapsed.TotalSeconds, 3)
        actual_duration_seconds = [Math]::Round($matrixStopwatch.Elapsed.TotalSeconds, 3)
        machine = $machineEvidence
        ffmpeg = [ordered]@{ root = $ffmpegRootPath; version = $ffmpegVersion; executable = if ($null -ne $ffmpegRootPath) { Join-Path $ffmpegRootPath 'bin\ffmpeg.exe' } else { $null } }
        scope = [ordered]@{ gui = 'not exercised'; live_audio_device = 'not exercised'; renderer_gpu = 'not exercised'; decoder_backends = $decoderBackends; encoder_backends = $encoderBackends; preview_output = 'scenario-specific; decoded cache pressure uses 160x90 RGBA' }
        run_count = $runs.Count; scenario_count = $totals.Count; total_scenario_runs = $totalScenarioRuns; runs = $runs; scenario_totals = $totals
    }
    try {
        New-Item -ItemType Directory -Force -Path $artifactRoot | Out-Null
        Write-AtomicJson $resolvedReportPath $wrapper
    } finally {
        if ($null -ne $phase0Mutex) { $phase0Mutex.ReleaseMutex(); $phase0Mutex.Dispose() }
    }
}
if ($null -ne $failure) { throw "Phase 0 sustained scenarios failed; preserved evidence: $resolvedReportPath. $failure" }
Write-Host "Phase 0 sustained scenarios: PASS ($resolvedReportPath; matrix duration $([Math]::Round($matrixStopwatch.Elapsed.TotalSeconds, 3)) s; runs $($runs.Count))"
