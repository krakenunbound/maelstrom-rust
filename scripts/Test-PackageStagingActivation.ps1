#requires -Version 7.0
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'Activate-PackageStaging.ps1')

$repoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$testRoot = Join-Path $repoRoot ('artifacts\package-activation-test-{0}' -f [guid]::NewGuid().ToString('N'))
try {
    New-Item -ItemType Directory -Path $testRoot -Force | Out-Null

    $live = Join-Path $testRoot 'Maelstrom-Windows-x64'
    $staging = Join-Path $testRoot '.Maelstrom-Windows-x64.staging-success'
    New-Item -ItemType Directory -Path $live, $staging | Out-Null
    Set-Content -LiteralPath (Join-Path $live 'identity.txt') -Value 'prior' -NoNewline
    Set-Content -LiteralPath (Join-Path $staging 'identity.txt') -Value 'candidate' -NoNewline
    Invoke-PackageStagingActivation -StagingDirectory $staging -LiveDirectory $live | Out-Null
    if ((Get-Content -LiteralPath (Join-Path $live 'identity.txt') -Raw) -ne 'candidate' -or
        (Test-Path -LiteralPath $staging) -or
        @(Get-ChildItem -LiteralPath $testRoot -Directory -Filter '.Maelstrom-Windows-x64.rollback-*').Count -ne 0) {
        throw 'Successful activation did not replace the live package atomically.'
    }

    $firstLive = Join-Path $testRoot 'Maelstrom-Windows-x64-first'
    $firstStaging = Join-Path $testRoot '.Maelstrom-Windows-x64-first.staging'
    New-Item -ItemType Directory -Path $firstStaging | Out-Null
    Set-Content -LiteralPath (Join-Path $firstStaging 'identity.txt') -Value 'first-package' -NoNewline
    Invoke-PackageStagingActivation -StagingDirectory $firstStaging -LiveDirectory $firstLive | Out-Null
    if ((Get-Content -LiteralPath (Join-Path $firstLive 'identity.txt') -Raw) -ne 'first-package' -or
        (Test-Path -LiteralPath $firstStaging) -or
        @(Get-ChildItem -LiteralPath $testRoot -Directory -Filter '.Maelstrom-Windows-x64-first.rollback-*').Count -ne 0) {
        throw 'First package activation did not create the live package cleanly.'
    }

    $rollbackLive = Join-Path $testRoot 'Maelstrom-Windows-x64-rollback'
    $rollbackStaging = Join-Path $testRoot '.Maelstrom-Windows-x64-rollback.staging-failure'
    New-Item -ItemType Directory -Path $rollbackLive, $rollbackStaging | Out-Null
    Set-Content -LiteralPath (Join-Path $rollbackLive 'identity.txt') -Value 'prior' -NoNewline
    Set-Content -LiteralPath (Join-Path $rollbackStaging 'identity.txt') -Value 'candidate' -NoNewline
    try {
        Invoke-PackageStagingActivation -StagingDirectory $rollbackStaging -LiveDirectory $rollbackLive -FailureInjector { throw 'injected activation failure' } | Out-Null
        throw 'Injected activation failure was accepted.'
    } catch {
        if ($_.Exception.Message -notmatch 'injected activation failure') { throw }
    }
    if ((Get-Content -LiteralPath (Join-Path $rollbackLive 'identity.txt') -Raw) -ne 'prior' -or
        (Get-Content -LiteralPath (Join-Path $rollbackStaging 'identity.txt') -Raw) -ne 'candidate' -or
        @(Get-ChildItem -LiteralPath $testRoot -Directory -Filter '.Maelstrom-Windows-x64-rollback.rollback-*').Count -ne 0) {
        throw 'Injected activation failure did not restore the prior live package.'
    }

    Write-Host 'Package staging activation: PASS'
} finally {
    if (Test-Path -LiteralPath $testRoot -PathType Container) {
        Remove-Item -LiteralPath $testRoot -Recurse -Force
    }
}
