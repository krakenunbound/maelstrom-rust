#requires -Version 7.0
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$manifestPath = Join-Path $repoRoot 'fixtures\media\manifest.json'
$validator = Join-Path $PSScriptRoot 'Test-MediaFixtures.ps1'
$hashBefore = (Get-FileHash -LiteralPath $manifestPath -Algorithm SHA256).Hash

$normalOutput = & $validator -ManifestOnly 2>&1
$normalExitCode = $LASTEXITCODE
if ($normalExitCode -ne 0) {
    throw "ManifestOnly audio coverage failed: $($normalOutput -join [Environment]::NewLine)"
}
Write-Output 'ManifestOnly audio coverage: PASS'

$fixtureOutput = @()
$fixtureRejected = $false
try {
    $fixtureOutput = @(& $validator -ManifestOnly -ManifestCoverageContractFixture 2>&1)
}
catch {
    $fixtureRejected = $true
    $fixtureOutput = @($_.Exception.Message)
}
if (-not $fixtureRejected) {
    throw 'Manifest coverage contract fixture unexpectedly passed.'
}
if (($fixtureOutput -join [Environment]::NewLine) -notmatch 'Manifest audio coverage requires exact mono') {
    throw 'Manifest coverage contract fixture failed for an unexpected reason.'
}

$hashAfter = (Get-FileHash -LiteralPath $manifestPath -Algorithm SHA256).Hash
if ($hashBefore -ne $hashAfter) {
    throw 'Manifest coverage contract mutated fixtures/media/manifest.json.'
}
Write-Output 'Manifest coverage contract fixture: PASS (rejected incomplete in-memory view; manifest hash unchanged)'
