#requires -Version 7.0
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'Assert-HardwareTransferTiming.ps1')

function New-Stage([object]$Samples, [object]$Total, [object]$Mean, [object]$Max) {
    [pscustomobject]@{ samples = $Samples; total_ms = $Total; mean_ms = $Mean; max_ms = $Max }
}
function Assert-Accepted([scriptblock]$Action, [string]$Name) {
    try { & $Action } catch { throw "$Name should pass: $($_.Exception.Message)" }
}
function Assert-Rejected([scriptblock]$Action, [string]$Name) {
    try { & $Action } catch { return }
    throw "$Name should reject."
}

$zero = New-Stage 0 0 0 0
$positive = New-Stage 2 3 1.5 2
Assert-Accepted { Assert-HardwareTransferTiming $zero @('Software') 'Software zero' } 'Software-zero'
Assert-Accepted { Assert-HardwareTransferTiming $zero @('Intel Quick Sync') 'Intel zero' } 'Intel-zero'
Assert-Accepted { Assert-HardwareTransferTiming $zero @('NVIDIA CUVID') 'NVIDIA zero' } 'NVIDIA-zero'
Assert-Rejected { Assert-HardwareTransferTiming $zero @('Windows D3D11VA') 'D3D11VA zero' } 'D3D11VA-zero'
Assert-Rejected { Assert-HardwareTransferTiming $zero @('Windows DXVA2') 'DXVA2 zero' } 'DXVA2-zero'
Assert-Rejected { Assert-HardwareTransferTiming $zero @('Apple VideoToolbox') 'VideoToolbox zero' } 'VideoToolbox-zero'
Assert-Accepted { Assert-HardwareTransferTiming $positive @('Windows DXVA2') 'DXVA2 positive' } 'DXVA2-positive'
Assert-Accepted { Assert-HardwareTransferTiming $positive @('Apple VideoToolbox') 'VideoToolbox positive' } 'VideoToolbox-positive'
Assert-Rejected { Assert-HardwareTransferTiming $zero @('Software', 'Windows D3D11VA') 'mixed zero' } 'mixed-zero'
Assert-Rejected { Assert-HardwareTransferTiming $zero @('Unknown decoder') 'unknown backend' } 'unknown-backend'
Assert-Rejected { Assert-HardwareTransferTiming $zero @('') 'blank backend' } 'blank-backend'
Assert-Rejected { Assert-HardwareTransferTiming $zero @() 'missing backend' } 'missing-backend'
Assert-Rejected { Assert-HardwareTransferTiming (New-Stage 1.5 1 1 1) @('Software') 'fractional samples' } 'malformed-samples'
Assert-Rejected { Assert-HardwareTransferTiming ([pscustomobject]@{ samples = 1; total_ms = 1; max_ms = 1 }) @('Software') 'missing mean' } 'malformed-stage'
Assert-Rejected { Assert-HardwareTransferTiming (New-Stage 2 3 1.4 2) @('Software') 'inconsistent mean' } 'mean-invariant'
Assert-Rejected { Assert-HardwareTransferTiming (New-Stage 0 1 0 0) @('Software') 'zero durations' } 'zero-duration-invariant'
Assert-Rejected { Assert-HardwareTransferTiming (New-Stage 1 ([double]::NaN) 0 0) @('Software') 'nonfinite duration' } 'finite-duration-invariant'
Assert-Rejected { Assert-HardwareTransferTiming (New-Stage 1 $true $true $true) @('Windows D3D11VA') 'boolean durations' } 'boolean-duration-types'
Assert-Rejected { Assert-HardwareTransferTiming (New-Stage 1 '1' '1' '1') @('Windows D3D11VA') 'string durations' } 'string-duration-types'

foreach ($validator in @('package-windows.ps1', 'Run-Phase0CrossAdapterSurface.ps1')) {
    $text = Get-Content -LiteralPath (Join-Path $PSScriptRoot $validator) -Raw
    if ($text -notmatch 'Assert-HardwareTransferTiming\.ps1' -or $text -notmatch 'Assert-HardwareTransferTiming') {
        throw "$validator does not statically integrate the hardware-transfer helper."
    }
}
Write-Host 'Hardware transfer timing contract: PASS'
