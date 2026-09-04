#requires -Version 7.0
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$repoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$runner = Join-Path $PSScriptRoot 'Run-Phase0CrossAdapterSurface.ps1'
$artifactRoot = Join-Path $repoRoot 'artifacts\phase0-cross-adapter-surface'
$sentinel = Join-Path $artifactRoot ('launcher-contract-sentinel-' + [Guid]::NewGuid().ToString('N') + '.json')
$sentinelText = '{"preserve":"validation-only"}'

try {
    New-Item -ItemType Directory -Path $artifactRoot -Force | Out-Null
    [IO.File]::WriteAllText($sentinel, $sentinelText, [Text.UTF8Encoding]::new($false))
    $validation = & (Join-Path $PSHOME 'pwsh.exe') -NoProfile -File $runner -ValidateOnly 2>&1
    if ($LASTEXITCODE -ne 0 -or ($validation -join [Environment]::NewLine) -notmatch 'validation') {
        throw "Launcher validation unexpectedly failed: $($validation -join [Environment]::NewLine)"
    }
    if (-not (Test-Path -LiteralPath $sentinel -PathType Leaf) -or
        [IO.File]::ReadAllText($sentinel, [Text.UTF8Encoding]::new($false)) -ne $sentinelText) {
        throw 'ValidateOnly modified existing evidence.'
    }
    $packagedExecutable = Join-Path $repoRoot 'dist\Maelstrom-Windows-x64\Maelstrom.exe'
    $rejection = & (Join-Path $PSHOME 'pwsh.exe') -NoProfile -File $runner -ValidateOnly -LauncherPath $packagedExecutable 2>&1
    if ($LASTEXITCODE -eq 0 -or ($rejection -join [Environment]::NewLine) -notmatch 'only permitted launcher') {
        throw 'Launcher contract accepted a packaged executable as a launcher.'
    }
    $text = Get-Content -LiteralPath $runner -Raw
    if ($text -match 'Start-Process\s+-FilePath\s+\$resolvedExecutable') {
        throw 'Launcher contract contains a direct packaged-editor Start-Process.'
    }
    if ($text -notmatch "MAELSTROM_LAUNCHER_WAIT\s*=\s*'1'" -or
        $text -notmatch 'Find-OwnedPackagedEditorProcess' -or
        $text -notmatch 'Start-Process\s+-FilePath\s+\$env:ComSpec') {
        throw 'Launcher contract omitted required batch-launch, wait, or process-tree binding behavior.'
    }
    Write-Host 'Phase 0 cross-adapter launcher contract: PASS'
} finally {
    Remove-Item -LiteralPath $sentinel -Force -ErrorAction SilentlyContinue
}
