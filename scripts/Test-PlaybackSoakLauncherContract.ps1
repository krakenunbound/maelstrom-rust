#requires -Version 7.0
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$repoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$runner = Join-Path $PSScriptRoot 'Run-PlaybackSoak.ps1'
$launcher = 'H:\Maelstrom Rust\Launch-Maelstrom-Editor.bat'
$packagedExe = Join-Path $repoRoot 'dist\Maelstrom-Windows-x64\Maelstrom.exe'
$artifactDirectory = Join-Path $repoRoot 'artifacts\phase0-playback-soak'
$sentinelPath = Join-Path $artifactDirectory 'playback-soak-report.json'
$sentinelContents = "launcher-contract-sentinel-$([Guid]::NewGuid().ToString('N'))"
$artifactDirectoryExisted = Test-Path -LiteralPath $artifactDirectory -PathType Container
$sentinelExisted = Test-Path -LiteralPath $sentinelPath -PathType Leaf
$originalSentinelBytes = if ($sentinelExisted) { [IO.File]::ReadAllBytes($sentinelPath) } else { $null }

$tokens = $null
$parseErrors = $null
[Management.Automation.Language.Parser]::ParseFile($runner, [ref]$tokens, [ref]$parseErrors) | Out-Null
if ($parseErrors.Count -ne 0) { throw "Playback soak runner has parser errors: $($parseErrors.Message -join '; ')" }

$source = Get-Content -LiteralPath $runner -Raw
if ($source -notmatch "\$env:MAELSTROM_LAUNCHER_WAIT = '1'" -or
    $source -notmatch 'Start-Process -FilePath \$env:ComSpec' -or
    $source -match 'Start-Process -FilePath \$resolvedExecutable') {
    throw 'Playback soak runner must use the waiting batch launcher rather than launching Maelstrom.exe directly.'
}

try {
    New-Item -ItemType Directory -Path $artifactDirectory -Force | Out-Null
    [IO.File]::WriteAllText($sentinelPath, $sentinelContents, [Text.UTF8Encoding]::new($false))

    $validation = & $runner -LauncherPath $launcher -ValidateOnly
    if ($validation.validation -ne 'passed' -or $validation.launch_performed -ne $false -or
        -not [string]::Equals($validation.launcher_path, $launcher, [StringComparison]::OrdinalIgnoreCase) -or
        -not [string]::Equals($validation.executable_path, [IO.Path]::GetFullPath($packagedExe), [StringComparison]::OrdinalIgnoreCase) -or
        [string]::IsNullOrWhiteSpace($validation.launcher_sha256) -or
        [string]::IsNullOrWhiteSpace($validation.executable_sha256) -or
        [IO.File]::ReadAllText($sentinelPath) -ne $sentinelContents) {
        throw 'Playback soak validation did not preserve launcher/runtime identity and existing report evidence.'
    }

    $wrongPathRejected = $false
    try {
        & $runner -LauncherPath $packagedExe -ValidateOnly | Out-Null
    } catch {
        $wrongPathRejected = $_.Exception.Message -like '*only permitted launcher*'
    }
    if (-not $wrongPathRejected -or [IO.File]::ReadAllText($sentinelPath) -ne $sentinelContents) {
        throw 'Playback soak runner accepted a packaged executable as its launcher or modified report evidence in validation mode.'
    }
} finally {
    if ($sentinelExisted) {
        [IO.File]::WriteAllBytes($sentinelPath, $originalSentinelBytes)
    } else {
        Remove-Item -LiteralPath $sentinelPath -Force -ErrorAction SilentlyContinue
    }
    if (-not $artifactDirectoryExisted -and (Test-Path -LiteralPath $artifactDirectory -PathType Container) -and
        @(Get-ChildItem -LiteralPath $artifactDirectory -Force -ErrorAction SilentlyContinue).Count -eq 0) {
        Remove-Item -LiteralPath $artifactDirectory -Force -ErrorAction SilentlyContinue
    }
}

Write-Host 'Playback soak launcher contract: PASS'
