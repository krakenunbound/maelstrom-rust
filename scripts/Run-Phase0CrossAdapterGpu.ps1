#requires -Version 7.0
[CmdletBinding()]
param(
    [string]$ReportPath
)

$ErrorActionPreference = 'Stop'

function Test-AbsolutePath([string]$Path) {
    [IO.Path]::IsPathRooted($Path) -and [string]::Equals([IO.Path]::GetFullPath($Path), $Path, [StringComparison]::OrdinalIgnoreCase)
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

$savedPath = $env:PATH
$savedFfmpeg = $env:FFMPEG_DIR
$savedLibclang = $env:LIBCLANG_PATH
$savedReport = $env:MAELSTROM_PHASE0_CROSS_ADAPTER_GPU_REPORT
$repoLocationPushed = $false
try {
    New-Item -ItemType Directory -Force -Path $artifactRoot | Out-Null
    Remove-Item -LiteralPath $resolvedReportPath -Force -ErrorAction SilentlyContinue
    $env:FFMPEG_DIR = $ffmpegRoot
    $env:LIBCLANG_PATH = $libclangRoot
    $env:PATH = $ffmpegBin + [IO.Path]::PathSeparator + $libclangRoot + [IO.Path]::PathSeparator + $savedPath
    $env:MAELSTROM_PHASE0_CROSS_ADAPTER_GPU_REPORT = $resolvedReportPath
    Push-Location -LiteralPath $repoRoot
    $repoLocationPushed = $true
    & $cargo test --release -p nle-render viewer_compositor::tests::phase0_cross_adapter_viewer_compositor_qualification -- --ignored --exact --test-threads=1
    $testExitCode = $LASTEXITCODE

    if (-not (Test-Path -LiteralPath $resolvedReportPath -PathType Leaf)) {
        throw "Cross-adapter qualification exited with code $testExitCode without writing its report. Both DX12 IntegratedGpu and DiscreteGpu adapters are required."
    }
    $report = Get-Content -LiteralPath $resolvedReportPath -Raw | ConvertFrom-Json
    if ($testExitCode -ne 0 -or $report.schema_version -ne 2 -or $report.status -ne 'passed' -or
        $report.scope -ne 'headless_transformed_multilayer_viewer_compositor' -or $report.physical_scanout_observed -ne $false -or
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
        throw "Cross-adapter report has an invalid schema-2 workload: $resolvedReportPath"
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
