#requires -Version 7.0
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$repoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$runner = Join-Path $PSScriptRoot 'Run-PlaybackDisruptions.ps1'
$launcher = 'H:\Maelstrom Rust\Launch-Maelstrom-Editor.bat'
$packagedExe = [IO.Path]::GetFullPath((Join-Path $repoRoot 'dist\Maelstrom-Windows-x64\Maelstrom.exe'))
$artifactDirectory = Join-Path $repoRoot 'artifacts\phase0-playback-disruptions'
$artifactPaths = @(
    (Join-Path $artifactDirectory 'deterministic-full-av-60s.mp4'),
    (Join-Path $artifactDirectory 'playback-disruptions-app-report.json'),
    (Join-Path $artifactDirectory 'playback-disruptions-report.json'),
    (Join-Path $artifactDirectory 'playback-disruptions-cancelled.mp4')
)

$tokens = $null; $parseErrors = $null
[Management.Automation.Language.Parser]::ParseFile($runner, [ref]$tokens, [ref]$parseErrors) | Out-Null
if ($parseErrors.Count -ne 0) { throw "Playback disruptions runner has parser errors: $($parseErrors.Message -join '; ')" }
$source = Get-Content -LiteralPath $runner -Raw
if ($source -notmatch "\$env:MAELSTROM_LAUNCHER_WAIT = '1'" -or $source -notmatch '--cache-mb=512' -or
    $source -notmatch 'snapshot_restore_audio_restarts' -or $source -notmatch 'cleanup_timeout' -or
    $source -notmatch 'export_job_settled' -or $source -notmatch 'export_residue_present' -or
    $source -notmatch 'terminal_cleanup_failure' -or
    $source -notmatch '\$appReport\.cleanup_timeout -ne \$false' -or
    $source -notmatch '\$appReport\.export_job_settled -ne \$true' -or
    $source -notmatch '\$appReport\.export_residue_present -ne \$false' -or
    $source -notmatch '\$null -ne \$appReport\.terminal_cleanup_failure' -or
    $source -notmatch 'Capture-OwnedProcessTree' -or $source -notmatch 'Test-OwnedProcessStillRunning' -or
    $source -notmatch 'Stop-OwnedProcessTree' -or $source -notmatch '\$exportPath, \$exportTemporaryPath' -or
    $source -notmatch 'Start-Process -FilePath \$env:ComSpec' -or $source -match 'Start-Process -FilePath \$resolvedExecutable') {
    throw 'Playback disruptions runner must require settled export cleanup, verified owned-process/artifact cleanup, frame-gated audio restarts, and the exact waiting batch launcher with --cache-mb=512.'
}
$cleanupGate = $source.IndexOf('if ($null -ne $primaryFailure -or $cleanupFailures.Count -ne 0)')
$passedWrite = $source.LastIndexOf('Write-AtomicUtf8File -Path $finalReportPath')
if ($cleanupGate -lt 0 -or $passedWrite -lt $cleanupGate) {
    throw 'Playback disruptions runner must publish a passed wrapper only after cleanup failures have been checked.'
}

$environmentNames = @('PATH', 'MAELSTROM_SMOKE_EDITOR', 'MAELSTROM_MEDIA_ACCEPTANCE_PATH', 'MAELSTROM_PLAYBACK_DISRUPTION_REPORT', 'MAELSTROM_PLAYBACK_DISRUPTION_EXPORT_PATH', 'MAELSTROM_LAUNCHER_WAIT')
$beforeEnvironment = @{}
foreach ($name in $environmentNames) { $beforeEnvironment[$name] = (Get-Item "Env:$name" -ErrorAction SilentlyContinue).Value }
$beforeArtifacts = @{}
$artifactDirectoryExisted = Test-Path -LiteralPath $artifactDirectory -PathType Container
foreach ($path in $artifactPaths) {
    $beforeArtifacts[$path] = if (Test-Path -LiteralPath $path -PathType Leaf) { (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash } else { $null }
}

$validation = & $runner -LauncherPath $launcher -ValidateOnly
if ($validation.validation -ne 'passed' -or $validation.launch_performed -ne $false -or
    -not [string]::Equals($validation.launcher_path, $launcher, [StringComparison]::OrdinalIgnoreCase) -or
    -not [string]::Equals($validation.executable_path, $packagedExe, [StringComparison]::OrdinalIgnoreCase) -or
    [string]::IsNullOrWhiteSpace($validation.launcher_sha256) -or [string]::IsNullOrWhiteSpace($validation.executable_sha256)) {
    throw 'Playback disruptions ValidateOnly did not prove the exact launcher and derived package identity.'
}
foreach ($name in $environmentNames) {
    if ((Get-Item "Env:$name" -ErrorAction SilentlyContinue).Value -ne $beforeEnvironment[$name]) { throw "ValidateOnly changed environment variable $name." }
}
foreach ($path in $artifactPaths) {
    $after = if (Test-Path -LiteralPath $path -PathType Leaf) { (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash } else { $null }
    if ($after -ne $beforeArtifacts[$path]) { throw "ValidateOnly created, deleted, or modified artifact $path." }
}
$afterDirectoryExisted = Test-Path -LiteralPath $artifactDirectory -PathType Container
if ($afterDirectoryExisted -ne $artifactDirectoryExisted) { throw 'ValidateOnly created or deleted the disruption artifact directory.' }
$wrongPathRejected = $false
try { & $runner -LauncherPath $packagedExe -ValidateOnly | Out-Null } catch { $wrongPathRejected = $_.Exception.Message -like '*only permitted launcher*' }
if (-not $wrongPathRejected) { throw 'Playback disruptions runner accepted Maelstrom.exe as a launcher.' }
foreach ($name in $environmentNames) {
    if ((Get-Item "Env:$name" -ErrorAction SilentlyContinue).Value -ne $beforeEnvironment[$name]) { throw "Rejected validation changed environment variable $name." }
}
if ((Test-Path -LiteralPath $artifactDirectory -PathType Container) -ne $artifactDirectoryExisted) { throw 'Rejected validation changed the disruption artifact directory.' }
Write-Host 'Playback disruptions launcher contract: PASS'
