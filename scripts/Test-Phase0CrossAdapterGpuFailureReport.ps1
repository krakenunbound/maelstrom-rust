#requires -Version 7.0
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$repoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$runner = Join-Path $PSScriptRoot 'Run-Phase0CrossAdapterGpu.ps1'
$artifactRoot = Join-Path $repoRoot 'artifacts\phase0-cross-adapter'
$reportPath = Join-Path $artifactRoot ('phase0-cross-adapter-gpu-failure-contract-' + [Guid]::NewGuid().ToString('N') + '.json')
$collisionPath = "$reportPath.tmp"
$artifactRootExisted = Test-Path -LiteralPath $artifactRoot -PathType Container

$runnerSource = Get-Content -LiteralPath $runner -Raw
$cargoStageIndex = $runnerSource.IndexOf("`$failureComponent = 'cargo'")
$cargoExitCheckIndex = $runnerSource.IndexOf('if ($testExitCode -ne 0)')
$reportValidationStageIndex = $runnerSource.IndexOf("`$failureComponent = 'report_validation'")
if ($cargoStageIndex -lt 0 -or $cargoExitCheckIndex -le $cargoStageIndex -or
    $reportValidationStageIndex -le $cargoExitCheckIndex) {
    throw 'Cargo nonzero failures must be classified before schema-3 report validation begins.'
}

function Get-ArtifactSnapshot([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path -PathType Container)) { return @() }
    return @(Get-ChildItem -LiteralPath $Path -File -Recurse -Force | ForEach-Object {
        '{0}|{1}' -f $_.FullName, (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash
    } | Sort-Object)
}

try {
    $beforeValidateOnly = Get-ArtifactSnapshot $artifactRoot
    $validateOutput = & (Join-Path $PSHOME 'pwsh.exe') -NoProfile -File $runner -ValidateOnly 2>&1
    if ($LASTEXITCODE -ne 0) { throw "ValidateOnly failed: $($validateOutput -join [Environment]::NewLine)" }
    $afterValidateOnly = Get-ArtifactSnapshot $artifactRoot
    if (($beforeValidateOnly -join "`n") -ne ($afterValidateOnly -join "`n")) {
        throw 'ValidateOnly created, deleted, or modified retained cross-adapter GPU evidence.'
    }

    New-Item -ItemType Directory -Force -Path $artifactRoot | Out-Null
    # A fixed-name writer would collide here. Publication must use its own unique same-directory temp.
    New-Item -ItemType Directory -Force -Path $collisionPath | Out-Null
    $runnerOutput = & (Join-Path $PSHOME 'pwsh.exe') -NoProfile -File $runner -ValidateOnly -FailureReportContractFixture -ReportPath $reportPath 2>&1
    if ($LASTEXITCODE -eq 0) { throw "Failure fixture unexpectedly passed: $($runnerOutput -join [Environment]::NewLine)" }
    if (-not (Test-Path -LiteralPath $reportPath -PathType Leaf)) { throw 'Failure fixture did not atomically publish the final report.' }

    $report = Get-Content -LiteralPath $reportPath -Raw | ConvertFrom-Json
    $expectedTopLevel = @('schema_version', 'status', 'scope', 'source_revision', 'report_path', 'available_adapter_inventory', 'machine', 'renderer_backend', 'renderer_driver', 'physical_scanout_observed', 'app_auto_preview_observed', 'failure') | Sort-Object
    $actualTopLevel = @($report.PSObject.Properties.Name | Sort-Object)
    if (($actualTopLevel -join '|') -ne ($expectedTopLevel -join '|') -or
        $report.schema_version -ne 1 -or $report.status -ne 'failed' -or
        $report.scope -ne 'headless_cross_adapter_viewer_compositor_qualification' -or
        $report.report_path -ne $reportPath -or $report.physical_scanout_observed -ne $false -or $report.app_auto_preview_observed -ne $false -or
        $null -ne $report.available_adapter_inventory -or $null -ne $report.machine -or
        $null -ne $report.renderer_backend -or $null -ne $report.renderer_driver) {
        throw 'Failure envelope top-level contract is invalid.'
    }
    if ($null -ne $report.source_revision -and [string]$report.source_revision -notmatch '^[0-9a-f]{40}$') {
        throw 'Failure envelope source_revision is neither null nor a full Git revision.'
    }
    $expectedFailure = @('component', 'stage', 'error_type', 'requested_device_type', 'process_exit_code', 'message') | Sort-Object
    if ((@($report.failure.PSObject.Properties.Name | Sort-Object) -join '|') -ne ($expectedFailure -join '|') -or
        $report.failure.component -ne 'renderer' -or $report.failure.stage -ne 'device_creation' -or
        $report.failure.error_type -ne 'System.Management.Automation.RuntimeException' -or
        $null -ne $report.failure.requested_device_type -or $null -ne $report.failure.process_exit_code -or
        [string]::IsNullOrWhiteSpace([string]$report.failure.message) -or $report.failure.message.Length -gt 512) {
        throw 'Failure envelope primary-failure contract is invalid.'
    }
    if (-not (Test-Path -LiteralPath $collisionPath -PathType Container)) { throw 'Sibling fixed-temp collision was modified.' }
    $staleTemps = @(Get-ChildItem -LiteralPath $artifactRoot -Filter ('.{0}.*.tmp' -f [IO.Path]::GetFileName($reportPath)) -Force)
    if ($staleTemps.Count -ne 0) { throw 'Atomic failure publication left a unique temporary file behind.' }
    Write-Host 'Phase 0 cross-adapter GPU failure-report contract: PASS'
} finally {
    Remove-Item -LiteralPath $reportPath -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $collisionPath -Recurse -Force -ErrorAction SilentlyContinue
    if (-not $artifactRootExisted) { Remove-Item -LiteralPath $artifactRoot -Force -ErrorAction SilentlyContinue }
}
