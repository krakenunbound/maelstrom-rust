#requires -Version 7.0
[CmdletBinding()]
param(
    [string]$ReportPath,
    [switch]$ValidateOnly,
    # Test-only, non-launching publication seam. It is rejected unless paired with ValidateOnly.
    [switch]$FailureReportContractFixture
)

$ErrorActionPreference = 'Stop'

function Test-AbsolutePath([string]$Path) {
    [IO.Path]::IsPathRooted($Path) -and [string]::Equals([IO.Path]::GetFullPath($Path), $Path, [StringComparison]::OrdinalIgnoreCase)
}

function Get-BoundedFailureMessage($ErrorRecord) {
    $message = [string]$ErrorRecord.Exception.Message
    if ($message.Length -gt 512) { return $message.Substring(0, 512) }
    return $message
}

function Write-AtomicUtf8File {
    param([Parameter(Mandatory = $true)][string]$Path, [Parameter(Mandatory = $true)][string]$Contents)
    $directory = [IO.Path]::GetDirectoryName($Path)
    $temporary = Join-Path $directory ('.{0}.{1}.{2}.tmp' -f [IO.Path]::GetFileName($Path), $PID, [Guid]::NewGuid().ToString('N'))
    try {
        [IO.File]::WriteAllText($temporary, $Contents, [Text.UTF8Encoding]::new($false))
        if (Test-Path -LiteralPath $Path -PathType Leaf) {
            [IO.File]::Replace($temporary, $Path, $null)
        } else {
            [IO.File]::Move($temporary, $Path)
        }
    } finally {
        Remove-Item -LiteralPath $temporary -Force -ErrorAction SilentlyContinue
    }
}

function Publish-FailedQualificationEnvelope {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)]$ErrorRecord,
        [Parameter(Mandatory = $true)][string]$Component,
        [Parameter(Mandatory = $true)][string]$Stage,
        $RequestedDeviceType,
        [Nullable[int]]$ProcessExitCode,
        $SourceRevision
    )
    New-Item -ItemType Directory -Force -Path ([IO.Path]::GetDirectoryName($Path)) | Out-Null
    $envelope = [ordered]@{
        schema_version = 1
        status = 'failed'
        scope = 'headless_cross_adapter_viewer_compositor_qualification'
        source_revision = $SourceRevision
        report_path = $Path
        available_adapter_inventory = $null
        machine = $null
        renderer_backend = $null
        renderer_driver = $null
        physical_scanout_observed = $false
        app_auto_preview_observed = $false
        failure = [ordered]@{
            component = $Component
            stage = $Stage
            error_type = $ErrorRecord.Exception.GetType().FullName
            requested_device_type = $RequestedDeviceType
            process_exit_code = $ProcessExitCode
            message = Get-BoundedFailureMessage $ErrorRecord
        }
    }
    Write-AtomicUtf8File -Path $Path -Contents (($envelope | ConvertTo-Json -Depth 4) + "`n")
}

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$artifactRoot = [IO.Path]::GetFullPath((Join-Path $repoRoot 'artifacts\phase0-cross-adapter'))
if ([string]::IsNullOrWhiteSpace($ReportPath)) {
    $ReportPath = Join-Path $artifactRoot 'phase0-cross-adapter-gpu.json'
}
$resolvedReportPath = if ([IO.Path]::IsPathRooted($ReportPath)) {
    [IO.Path]::GetFullPath($ReportPath)
} else {
    [IO.Path]::GetFullPath((Join-Path $repoRoot $ReportPath))
}
if (-not [string]::Equals([IO.Path]::GetDirectoryName($resolvedReportPath), $artifactRoot, [StringComparison]::OrdinalIgnoreCase) -or
    [IO.Path]::GetExtension($resolvedReportPath) -ine '.json') {
    throw "Report output must be a JSON file directly inside the ignored artifact directory: $artifactRoot"
}

$sourceRevision = $null
try {
    $revisionCandidate = (& git.exe -C $repoRoot rev-parse --verify HEAD 2>$null | Select-Object -First 1)
    if ($revisionCandidate -match '^[0-9a-f]{40}$') { $sourceRevision = [string]$revisionCandidate }
} catch {}
$failureComponent = 'preflight'
$failureStage = 'dependencies'
$requestedDeviceType = $null
$processExitCode = $null
try {
if ($FailureReportContractFixture) {
    if (-not $ValidateOnly) {
        $failureComponent = 'runner'
        $failureStage = 'fixture_contract'
        throw 'FailureReportContractFixture is permitted only with -ValidateOnly.'
    }
    $failureComponent = 'renderer'
    $failureStage = 'device_creation'
    throw 'Deterministic test fixture: renderer device creation failed before adapter inventory was available.'
}
$ffmpegRoot = [IO.Path]::GetFullPath((Join-Path $repoRoot '.deps\ffmpeg-project-8.1'))
$ffmpegBin = Join-Path $ffmpegRoot 'bin'
$libclangCandidates = @(
    $env:LIBCLANG_PATH,
    (Join-Path $repoRoot '.deps\libclang-bindgen'),
    'C:\CraftRoot\bin'
) | Where-Object { -not [string]::IsNullOrWhiteSpace($_) }
$libclangRoot = $libclangCandidates | Where-Object {
    Test-Path -LiteralPath (Join-Path $_ 'libclang.dll') -PathType Leaf
} | Select-Object -First 1
$cargo = 'C:\Users\The Kraken\.cargo\bin\cargo.exe'
if ([string]::IsNullOrWhiteSpace($libclangRoot)) {
    throw 'Missing local libclang required by native FFmpeg bindings. Checked LIBCLANG_PATH, .deps\libclang-bindgen, and C:\CraftRoot\bin.'
}
foreach ($required in @(
    (Join-Path $ffmpegBin 'ffmpeg.exe'),
    (Join-Path $libclangRoot 'libclang.dll'),
    $cargo
)) {
    if (-not (Test-Path -LiteralPath $required -PathType Leaf)) {
        throw "Required repository-local Phase 0 runtime dependency is missing: $required"
    }
}
if (-not (Test-AbsolutePath $cargo)) { throw 'Cargo must be an absolute path.' }

if ($ValidateOnly) {
    Write-Host "Phase 0 cross-adapter compositor qualification contract: PASS ($resolvedReportPath)"
    return
}

$savedPath = $env:PATH
$savedFfmpeg = $env:FFMPEG_DIR
$savedLibclang = $env:LIBCLANG_PATH
$savedReport = $env:MAELSTROM_PHASE0_CROSS_ADAPTER_GPU_REPORT
$repoLocationPushed = $false
try {
    New-Item -ItemType Directory -Force -Path $artifactRoot | Out-Null
    $failureComponent = 'runner'
    $failureStage = 'report_output_preparation'
    $reportHashBefore = if (Test-Path -LiteralPath $resolvedReportPath -PathType Leaf) {
        (Get-FileHash -LiteralPath $resolvedReportPath -Algorithm SHA256).Hash
    } else { $null }
    $env:FFMPEG_DIR = $ffmpegRoot
    $env:LIBCLANG_PATH = $libclangRoot
    $env:PATH = $ffmpegBin + [IO.Path]::PathSeparator + $libclangRoot + [IO.Path]::PathSeparator + $savedPath
    $env:MAELSTROM_PHASE0_CROSS_ADAPTER_GPU_REPORT = $resolvedReportPath
    Push-Location -LiteralPath $repoRoot
    $repoLocationPushed = $true
    $failureComponent = 'cargo'
    $failureStage = 'qualification_test'
    & $cargo test --release -p nle-render viewer_compositor::tests::phase0_cross_adapter_viewer_compositor_qualification -- --ignored --exact --test-threads=1
    $testExitCode = $LASTEXITCODE
    $processExitCode = [int]$testExitCode
    if ($testExitCode -ne 0) {
        throw "Cross-adapter qualification process exited with code $testExitCode."
    }

    $failureComponent = 'report_validation'
    $failureStage = 'schema3_success_payload'
    if (-not (Test-Path -LiteralPath $resolvedReportPath -PathType Leaf)) {
        throw "Cross-adapter qualification exited with code $testExitCode without writing its report. Both DX12 IntegratedGpu and DiscreteGpu adapters are required."
    }
    if ($testExitCode -eq 0 -and $null -ne $reportHashBefore -and
        (Get-FileHash -LiteralPath $resolvedReportPath -Algorithm SHA256).Hash -eq $reportHashBefore) {
        throw 'Cross-adapter qualification exited successfully without replacing the prior report.'
    }
    $report = Get-Content -LiteralPath $resolvedReportPath -Raw | ConvertFrom-Json
    if ($report.schema_version -ne 3 -or $report.status -ne 'passed' -or
        $report.scope -ne 'headless_transformed_multilayer_viewer_compositor_with_post_measurement_state_scenarios' -or $report.physical_scanout_observed -ne $false -or
        $report.app_auto_preview_observed -ne $false -or $null -eq $report.machine -or $null -eq $report.workload -or
        @($report.adapters).Count -ne 2) {
        throw "Cross-adapter qualification failed or has an invalid report shape: $resolvedReportPath"
    }
    $workload = $report.workload
    if ([int]$workload.source_width -ne 1920 -or [int]$workload.source_height -ne 1080 -or
        [int]$workload.output_width -ne 1920 -or [int]$workload.output_height -ne 1080 -or
        $workload.sampling -ne 'Bicubic' -or [int]$workload.warmup_submissions -ne 5 -or
        [int]$workload.measured_submissions -ne 30 -or [int]$workload.target_fps -ne 30 -or
        -not [bool]$workload.uploads_excluded_from_timing -or -not [bool]$workload.warmup_excluded_from_timing -or
        -not [double]::IsFinite([double]$workload.frame_budget_ms) -or [math]::Abs([double]$workload.frame_budget_ms - (1000.0 / 30.0)) -gt 0.001) {
        throw "Cross-adapter report has an invalid schema-3 workload: $resolvedReportPath"
    }
    $deviceTypes = @($report.adapters | ForEach-Object { [string]$_.device_type })
    foreach ($requiredType in @('IntegratedGpu', 'DiscreteGpu')) {
        if ($deviceTypes -notcontains $requiredType) {
            throw "Cross-adapter report is missing the required DX12 $requiredType evidence: $resolvedReportPath"
        }
    }
    foreach ($adapter in @($report.adapters)) {
        if ([string]::IsNullOrWhiteSpace([string]$adapter.name) -or [string]::IsNullOrWhiteSpace([string]$adapter.backend) -or
            $adapter.backend -ne 'Dx12' -or -not [bool]$adapter.correctness_readback_passed -or
            -not [bool]$adapter.timestamp_query_supported -or [int]$adapter.warmup_submissions -ne [int]$workload.warmup_submissions -or
            [int]$adapter.measured_submissions -ne [int]$workload.measured_submissions -or $null -eq $adapter.cpu_encode_timing -or
            $null -eq $adapter.gpu_pass_timing -or $null -eq $adapter.correctness_actual_rgba -or
            $null -eq $adapter.correctness_expected_rgba -or [int]$adapter.correctness_tolerance -ne 4) {
            throw "Cross-adapter report has incomplete compositor evidence for adapter '$($adapter.name)': $resolvedReportPath"
        }
        $expectedLayerCount = if ($adapter.device_type -eq 'IntegratedGpu') { 2 } elseif ($adapter.device_type -eq 'DiscreteGpu') { 4 } else { 0 }
        if ([int]$adapter.layer_count -ne $expectedLayerCount) {
            throw "Cross-adapter report has the wrong layer count for adapter '$($adapter.name)': $resolvedReportPath"
        }
        foreach ($timingName in @('cpu_encode_timing', 'gpu_pass_timing')) {
            $timing = $adapter.$timingName
            if ([int]$timing.samples -ne [int]$workload.measured_submissions -or
                -not [double]::IsFinite([double]$timing.p95_ms) -or -not [double]::IsFinite([double]$timing.max_ms) -or
                [double]$timing.p95_ms -lt 0 -or [double]$timing.max_ms -lt [double]$timing.p95_ms) {
                throw "Cross-adapter report has invalid $timingName timing for adapter '$($adapter.name)': $resolvedReportPath"
            }
        }
        if ([double]$adapter.cpu_encode_timing.p95_ms -gt 8.0 -or
            [double]$adapter.gpu_pass_timing.p95_ms -gt [double]$workload.frame_budget_ms) {
            throw "Cross-adapter report exceeded the 30fps qualification budget for adapter '$($adapter.name)': $resolvedReportPath"
        }
        $actual = @($adapter.correctness_actual_rgba)
        $expected = @($adapter.correctness_expected_rgba)
        if ($actual.Count -ne 4 -or $expected.Count -ne 4) {
            throw "Cross-adapter report has invalid correctness RGBA for adapter '$($adapter.name)': $resolvedReportPath"
        }
        for ($index = 0; $index -lt 4; $index++) {
            if ([math]::Abs([int]$actual[$index] - [int]$expected[$index]) -gt [int]$adapter.correctness_tolerance) {
                throw "Cross-adapter report correctness verification exceeded tolerance for adapter '$($adapter.name)': $resolvedReportPath"
            }
        }
        $scenarios = @($adapter.state_scenarios)
        $scenarioNames = @('top_transform_off_center', 'top_layer_disabled', 'top_source_missing', 'top_source_late_arrival')
        $scenarioFields = @('name', 'generation', 'actual_rgba', 'expected_rgba', 'correctness_passed', 'probes', 'uploads_performed', 'upload_serials_before', 'upload_serials_after', 'composed_upload_serials', 'composition_matches_current_uploads', 'top_layer_composed')
        $probeFields = @('x', 'y', 'actual_rgba', 'expected_rgba', 'correctness_passed')
        if ($scenarios.Count -ne $scenarioNames.Count -or @($scenarios | ForEach-Object { [string]$_.name }) -join '|' -ne ($scenarioNames -join '|')) {
            throw "Cross-adapter report has an invalid state-scenario shape for adapter '$($adapter.name)': $resolvedReportPath"
        }
        $baselineSerials = @($scenarios[0].upload_serials_before)
        if ($baselineSerials.Count -ne 4 -or $baselineSerials[$expectedLayerCount - 1] -le 0) {
            throw "Cross-adapter report is missing initial top-layer upload evidence for adapter '$($adapter.name)': $resolvedReportPath"
        }
        for ($scenarioIndex = 0; $scenarioIndex -lt $scenarios.Count; $scenarioIndex++) {
            $scenario = $scenarios[$scenarioIndex]
            $actual = @($scenario.actual_rgba)
            $expected = @($scenario.expected_rgba)
            $before = @($scenario.upload_serials_before)
            $after = @($scenario.upload_serials_after)
            $composed = @($scenario.composed_upload_serials)
            $probes = @($scenario.probes)
            $expectedProbeCount = if ($scenarioIndex -eq 0) { 2 } else { 1 }
            $actualFields = @($scenario.PSObject.Properties.Name | Sort-Object)
            if ((($actualFields -join '|') -ne (($scenarioFields | Sort-Object) -join '|')) -or
                $scenario.correctness_passed -isnot [bool] -or $scenario.uploads_performed -isnot [bool] -or
                $scenario.composition_matches_current_uploads -isnot [bool] -or $scenario.top_layer_composed -isnot [bool] -or
                [int64]$scenario.generation -ne ([int64]$workload.warmup_submissions + [int64]$workload.measured_submissions + 1 + $scenarioIndex) -or
                $actual.Count -ne 4 -or $expected.Count -ne 4 -or $before.Count -ne 4 -or $after.Count -ne 4 -or $composed.Count -ne 4 -or
                $probes.Count -ne $expectedProbeCount -or
                -not [bool]$scenario.correctness_passed -or -not [bool]$scenario.composition_matches_current_uploads) {
                throw "Cross-adapter report has incomplete state-scenario evidence for adapter '$($adapter.name)': $resolvedReportPath"
            }
            for ($index = 0; $index -lt 4; $index++) {
                if ([math]::Abs([int]$actual[$index] - [int]$expected[$index]) -gt [int]$adapter.correctness_tolerance) {
                    throw "Cross-adapter state scenario '$($scenario.name)' exceeded readback tolerance for adapter '$($adapter.name)': $resolvedReportPath"
                }
            }
            foreach ($probe in $probes) {
                $probeActual = @($probe.actual_rgba)
                $probeExpected = @($probe.expected_rgba)
                if ((@($probe.PSObject.Properties.Name | Sort-Object) -join '|') -ne (($probeFields | Sort-Object) -join '|') -or
                    $probe.correctness_passed -isnot [bool] -or -not [bool]$probe.correctness_passed -or
                    $probeActual.Count -ne 4 -or $probeExpected.Count -ne 4) {
                    throw "Cross-adapter report has an invalid readback probe for adapter '$($adapter.name)': $resolvedReportPath"
                }
                for ($channel = 0; $channel -lt 4; $channel++) {
                    if ([math]::Abs([int]$probeActual[$channel] - [int]$probeExpected[$channel]) -gt [int]$adapter.correctness_tolerance) {
                        throw "Cross-adapter probe exceeded readback tolerance for adapter '$($adapter.name)': $resolvedReportPath"
                    }
                }
            }
        }
        $transform = $scenarios[0]
        $transformCenter = @($transform.probes)[0]
        $transformMoved = @($transform.probes)[1]
        if ([bool]$transform.uploads_performed -or -not [bool]$transform.top_layer_composed -or
            ((@($transform.upload_serials_before) -join ',') -ne (@($transform.upload_serials_after) -join ',')) -or
            @($transform.composed_upload_serials)[$expectedLayerCount - 1] -ne @($transform.upload_serials_after)[$expectedLayerCount - 1] -or
            [int]$transformCenter.x -ne 960 -or [int]$transformCenter.y -ne 540 -or
            [int]$transformMoved.x -ne 1800 -or [int]$transformMoved.y -ne 120 -or
            ((@($transformCenter.actual_rgba) -join ',') -ne (@($transform.actual_rgba) -join ',')) -or
            ((@($transformCenter.expected_rgba) -join ',') -ne (@($transform.expected_rgba) -join ',')) -or
            ((@($transformMoved.expected_rgba) -join ',') -ne (@($adapter.correctness_expected_rgba) -join ','))) {
            throw "Cross-adapter transform scenario did not prove retained-texture recomposition for adapter '$($adapter.name)': $resolvedReportPath"
        }
        $disabled = $scenarios[1]
        if ([bool]$disabled.uploads_performed -or [bool]$disabled.top_layer_composed -or
            ((@($disabled.upload_serials_before) -join ',') -ne (@($disabled.upload_serials_after) -join ','))) {
            throw "Cross-adapter disabled-layer scenario did not prove no-upload recomposition for adapter '$($adapter.name)': $resolvedReportPath"
        }
        $missing = $scenarios[2]
        $missingBefore = @($missing.upload_serials_before)
        $missingAfter = @($missing.upload_serials_after)
        $missingComposed = @($missing.composed_upload_serials)
        if ([bool]$missing.uploads_performed -or [bool]$missing.top_layer_composed -or
            $missingAfter[$expectedLayerCount - 1] -ne 0 -or $missingBefore[$expectedLayerCount - 1] -le 0) {
            throw "Cross-adapter missing-source scenario did not prove ready-layer composition without a new upload for adapter '$($adapter.name)': $resolvedReportPath"
        }
        for ($index = 0; $index -lt ($expectedLayerCount - 1); $index++) {
            if ($missingBefore[$index] -ne $missingAfter[$index] -or $missingComposed[$index] -eq $null) {
                throw "Cross-adapter missing-source scenario lost ready-layer evidence for adapter '$($adapter.name)': $resolvedReportPath"
            }
        }
        $late = $scenarios[3]
        $lateBefore = @($late.upload_serials_before)
        $lateAfter = @($late.upload_serials_after)
        $lateComposed = @($late.composed_upload_serials)
        if (-not [bool]$late.uploads_performed -or -not [bool]$late.top_layer_composed -or
            $lateBefore[$expectedLayerCount - 1] -ne 0 -or
            $lateAfter[$expectedLayerCount - 1] -le $baselineSerials[$expectedLayerCount - 1] -or
            $lateComposed[$expectedLayerCount - 1] -ne $lateAfter[$expectedLayerCount - 1]) {
            throw "Cross-adapter late-arrival scenario did not restore the top layer for adapter '$($adapter.name)': $resolvedReportPath"
        }
        if ((@($scenarios[0].expected_rgba) -join ',') -ne (@($scenarios[1].expected_rgba) -join ',') -or
            (@($scenarios[0].expected_rgba) -join ',') -ne (@($scenarios[2].expected_rgba) -join ',') -or
            (@($late.expected_rgba) -join ',') -ne (@($adapter.correctness_expected_rgba) -join ',')) {
            throw "Cross-adapter state scenarios do not have the required remaining/full composition expectations for adapter '$($adapter.name)': $resolvedReportPath"
        }
    }
    Write-Host "Phase 0 cross-adapter compositor qualification: PASS ($resolvedReportPath)"
}
finally {
    if ($repoLocationPushed) { Pop-Location }
    $env:PATH = $savedPath
    if ($null -eq $savedFfmpeg) { Remove-Item Env:FFMPEG_DIR -ErrorAction SilentlyContinue } else { $env:FFMPEG_DIR = $savedFfmpeg }
    if ($null -eq $savedLibclang) { Remove-Item Env:LIBCLANG_PATH -ErrorAction SilentlyContinue } else { $env:LIBCLANG_PATH = $savedLibclang }
    if ($null -eq $savedReport) { Remove-Item Env:MAELSTROM_PHASE0_CROSS_ADAPTER_GPU_REPORT -ErrorAction SilentlyContinue } else { $env:MAELSTROM_PHASE0_CROSS_ADAPTER_GPU_REPORT = $savedReport }
}
} catch {
    $operationError = $_
    try {
        Publish-FailedQualificationEnvelope -Path $resolvedReportPath -ErrorRecord $operationError -Component $failureComponent -Stage $failureStage -RequestedDeviceType $requestedDeviceType -ProcessExitCode $processExitCode -SourceRevision $sourceRevision
    } catch {
        Write-Warning "Could not publish Phase 0 cross-adapter failure envelope: $($_.Exception.Message)"
    }
    throw $operationError
}
