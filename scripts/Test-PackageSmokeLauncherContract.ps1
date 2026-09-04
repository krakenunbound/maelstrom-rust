#requires -Version 7.0
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$runner = Join-Path $PSScriptRoot 'package-windows.ps1'
$tokens = $null
$parseErrors = $null
[Management.Automation.Language.Parser]::ParseFile($runner, [ref]$tokens, [ref]$parseErrors) | Out-Null
if ($parseErrors.Count -ne 0) {
    throw "Package runner has parser errors: $($parseErrors.Message -join '; ')"
}

$source = Get-Content -LiteralPath $runner -Raw
$required = @(
    "\$approvedLauncherPath = 'H:\\Maelstrom Rust\\Launch-Maelstrom-Editor\.bat'",
    "\$env:MAELSTROM_LAUNCHER_WAIT = '1'",
    'Start-Process -FilePath \$env:ComSpec',
    'Find-OwnedPackagedEditorProcess',
    'taskkill\.exe" /PID \$launcherProcess\.Id /T /F'
)
foreach ($pattern in $required) {
    if ($source -notmatch $pattern) {
        throw "Package smoke launcher contract is missing: $pattern"
    }
}

if ($source -match "Start-Process -FilePath \(Join-Path \$output 'Maelstrom\.exe'\)" -or
    $source -match 'Start-Process -FilePath \$packageExePath') {
    throw 'Package smoke must not start the packaged executable directly.'
}

Write-Host 'Package smoke launcher contract: PASS'
