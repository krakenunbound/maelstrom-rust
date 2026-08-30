#requires -Version 7.0
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$repoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$runner = Join-Path $PSScriptRoot 'Run-Phase0CrossAdapterSurface.ps1'
$artifactRoot = Join-Path $repoRoot 'artifacts\phase0-cross-adapter-surface'
$testRoot = Join-Path $artifactRoot ("failure-contract-" + [Guid]::NewGuid().ToString('N'))
$executable = Join-Path $testRoot 'Maelstrom.exe'
$reportPath = Join-Path $artifactRoot 'phase0-cross-adapter-failure-contract.json'

try {
    New-Item -ItemType Directory -Path $testRoot -Force | Out-Null
    [IO.File]::WriteAllBytes($executable, [byte[]]::new(0))
    Remove-Item -LiteralPath $reportPath -Force -ErrorAction SilentlyContinue
    # A legacy fixed-temp writer collided with this sibling. The production writer must use a
    # unique same-directory temporary file and still publish the requested report atomically.
    New-Item -ItemType Directory -Path "$reportPath.tmp" -Force | Out-Null

    $runnerOutput = & (Join-Path $PSHOME 'pwsh.exe') -NoProfile -File $runner -ExecutablePath $executable -ReportPath $reportPath 2>&1
    $runnerExitCode = $LASTEXITCODE
    if ($runnerExitCode -eq 0) {
        throw "Failure-contract fixture unexpectedly passed: $($runnerOutput -join [Environment]::NewLine)"
    }
    if (-not (Test-Path -LiteralPath $reportPath -PathType Leaf)) {
        throw 'Failure-contract fixture did not write its combined report.'
    }

    $report = Get-Content -LiteralPath $reportPath -Raw | ConvertFrom-Json
    if ($report.schema_version -ne 2 -or $report.status -ne 'failed' -or
        $report.failure.component -ne 'package' -or $report.failure.stage -ne 'runtime_closure' -or
        $report.fixture.video_codec -ne 'mpeg4' -or $report.fixture.audio_codec -ne 'aac' -or
        $null -ne $report.failure.affected_codecs -or
        $null -ne $report.failure.renderer_backend -or $null -ne $report.failure.renderer_driver -or
        $null -ne $report.failure.renderer_driver_info -or $null -ne $report.failure.decoder_backends -or
        $null -ne $report.failure.encoder_backend) {
        throw 'Failure-contract report did not preserve the schema-2 package-runtime diagnosis.'
    }
    Write-Host 'Phase 0 failure-report contract: PASS'
} finally {
    Remove-Item -LiteralPath $reportPath -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath "$reportPath.tmp" -Force -Recurse -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $testRoot -Force -Recurse -ErrorAction SilentlyContinue
}
