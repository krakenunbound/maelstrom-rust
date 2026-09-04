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

$imageOutput = @()
$imageRejected = $false
try {
    $imageOutput = @(& $validator -ManifestOnly -ManifestImageContractFixture 2>&1)
}
catch {
    $imageRejected = $true
    $imageOutput = @($_.Exception.Message)
}
if (-not $imageRejected) {
    throw 'Manifest image contract fixture unexpectedly passed.'
}
if (($imageOutput -join [Environment]::NewLine) -notmatch 'Image fixture is missing pixel_format') {
    throw 'Manifest image contract fixture failed for an unexpected reason.'
}
if ($hashBefore -ne (Get-FileHash -LiteralPath $manifestPath -Algorithm SHA256).Hash) {
    throw 'Manifest image contract fixture mutated fixtures/media/manifest.json.'
}
Write-Output 'Manifest image contract fixture: PASS (rejected incomplete in-memory image metadata; manifest hash unchanged)'

$fourKOutput = @()
$fourKRejected = $false
try {
    $fourKOutput = @(& $validator -ManifestOnly -Manifest4kCoverageContractFixture 2>&1)
}
catch {
    $fourKRejected = $true
    $fourKOutput = @($_.Exception.Message)
}
if (-not $fourKRejected) {
    throw 'Manifest 4K coverage contract fixture unexpectedly passed.'
}
if (($fourKOutput -join [Environment]::NewLine) -notmatch 'Manifest video coverage requires at least one 4K-class fixture') {
    throw 'Manifest 4K coverage contract fixture failed for an unexpected reason.'
}
if ($hashBefore -ne (Get-FileHash -LiteralPath $manifestPath -Algorithm SHA256).Hash) {
    throw 'Manifest 4K coverage contract fixture mutated fixtures/media/manifest.json.'
}
Write-Output 'Manifest 4K coverage contract fixture: PASS (rejected in-memory 4K removal; manifest hash unchanged)'

$eightKOutput = @()
$eightKRejected = $false
try {
    $eightKOutput = @(& $validator -ManifestOnly -Manifest8kCoverageContractFixture 2>&1)
}
catch {
    $eightKRejected = $true
    $eightKOutput = @($_.Exception.Message)
}
if (-not $eightKRejected) {
    throw 'Manifest 8K coverage contract fixture unexpectedly passed.'
}
if (($eightKOutput -join [Environment]::NewLine) -notmatch 'Manifest video coverage requires at least one 8K-class fixture') {
    throw 'Manifest 8K coverage contract fixture failed for an unexpected reason.'
}
if ($hashBefore -ne (Get-FileHash -LiteralPath $manifestPath -Algorithm SHA256).Hash) {
    throw 'Manifest 8K coverage contract fixture mutated fixtures/media/manifest.json.'
}
Write-Output 'Manifest 8K coverage contract fixture: PASS (rejected in-memory 8K removal; manifest hash unchanged)'

$softwareDecodeOutput = @()
$softwareDecodeRejected = $false
try {
    $softwareDecodeOutput = @(& $validator -ManifestOnly -ManifestSoftwareDecodeContractFixture 2>&1)
}
catch {
    $softwareDecodeRejected = $true
    $softwareDecodeOutput = @($_.Exception.Message)
}
if (-not $softwareDecodeRejected) {
    throw 'Manifest software-decode schema fixture unexpectedly passed.'
}
if (($softwareDecodeOutput -join [Environment]::NewLine) -notmatch 'Software decode frame contract is invalid') {
    throw 'Manifest software-decode schema fixture failed for an unexpected reason.'
}
if ($hashBefore -ne (Get-FileHash -LiteralPath $manifestPath -Algorithm SHA256).Hash) {
    throw 'Manifest software-decode schema fixture mutated fixtures/media/manifest.json.'
}
Write-Output 'Manifest software-decode schema fixture: PASS (rejected non-integer frame count; manifest hash unchanged)'

$fractionalDecodeOutput = @()
$fractionalDecodeRejected = $false
try {
    $fractionalDecodeOutput = @(& $validator -ManifestOnly -ManifestSoftwareDecodeFractionalContractFixture 2>&1)
}
catch {
    $fractionalDecodeRejected = $true
    $fractionalDecodeOutput = @($_.Exception.Message)
}
if (-not $fractionalDecodeRejected) {
    throw 'Manifest fractional software-decode schema fixture unexpectedly passed.'
}
if (($fractionalDecodeOutput -join [Environment]::NewLine) -notmatch 'Software decode frame contract is invalid') {
    throw 'Manifest fractional software-decode schema fixture failed for an unexpected reason.'
}
if ($hashBefore -ne (Get-FileHash -LiteralPath $manifestPath -Algorithm SHA256).Hash) {
    throw 'Manifest fractional software-decode schema fixture mutated fixtures/media/manifest.json.'
}
Write-Output 'Manifest fractional software-decode schema fixture: PASS (rejected fractional frame count; manifest hash unchanged)'

$localCorpusOutput = @()
$localCorpusRejected = $false
try {
    $localCorpusOutput = @(& $validator -ManifestOnly -ManifestLocalCorpusContractSchemaFixture 2>&1)
}
catch {
    $localCorpusRejected = $true
    $localCorpusOutput = @($_.Exception.Message)
}
if (-not $localCorpusRejected) {
    throw 'Manifest local-corpus schema contract fixture unexpectedly passed.'
}
if (($localCorpusOutput -join [Environment]::NewLine) -notmatch 'Required local video picture-type count is invalid') {
    throw 'Manifest local-corpus schema contract fixture failed for an unexpected reason.'
}
if ($hashBefore -ne (Get-FileHash -LiteralPath $manifestPath -Algorithm SHA256).Hash) {
    throw 'Manifest local-corpus schema contract fixture mutated fixtures/media/manifest.json.'
}
Write-Output 'Manifest local-corpus schema contract fixture: PASS (rejected in-memory metadata; manifest hash unchanged)'

$localDurationOutput = @()
$localDurationRejected = $false
try {
    $localDurationOutput = @(& $validator -ManifestOnly -ManifestLocalCorpusDurationSchemaFixture 2>&1)
}
catch {
    $localDurationRejected = $true
    $localDurationOutput = @($_.Exception.Message)
}
if (-not $localDurationRejected) {
    throw 'Manifest local-corpus duration schema fixture unexpectedly passed.'
}
if (($localDurationOutput -join [Environment]::NewLine) -notmatch 'Required local fixture identity is invalid') {
    throw 'Manifest local-corpus duration schema fixture failed for an unexpected reason.'
}
if ($hashBefore -ne (Get-FileHash -LiteralPath $manifestPath -Algorithm SHA256).Hash) {
    throw 'Manifest local-corpus duration schema fixture mutated fixtures/media/manifest.json.'
}
Write-Output 'Manifest local-corpus duration schema fixture: PASS (rejected non-numeric in-memory duration; manifest hash unchanged)'
