function Test-HardwareTransferFiniteNonnegativeNumber {
    param($Value)

    if ($null -eq $Value -or $Value -is [bool] -or
        $Value -isnot [byte] -and $Value -isnot [sbyte] -and
        $Value -isnot [int16] -and $Value -isnot [uint16] -and
        $Value -isnot [int32] -and $Value -isnot [uint32] -and
        $Value -isnot [int64] -and $Value -isnot [uint64] -and
        $Value -isnot [single] -and $Value -isnot [double] -and $Value -isnot [decimal]) {
        return $false
    }
    try { $number = [double]$Value } catch { return $false }
    return -not [double]::IsNaN($number) -and
        -not [double]::IsInfinity($number) -and
        $number -ge 0
}

function Test-HardwareTransferUnsignedInteger {
    param($Value)

    if (-not (Test-HardwareTransferFiniteNonnegativeNumber $Value)) { return $false }
    try {
        $number = [double]$Value
        $integer = [uint64]$Value
    } catch {
        return $false
    }
    return $number -eq [double]$integer
}

function Assert-HardwareTransferTiming {
    param(
        [Parameter(Mandatory = $true)]$Stage,
        [Parameter(Mandatory = $true)][object[]]$DecoderBackends,
        [Parameter(Mandatory = $true)][string]$Context
    )

    $knownDecodeBackendTransferRequirements = @{
        'Apple VideoToolbox' = $true
        'Windows D3D11VA' = $true
        'Windows DXVA2' = $true
        'Software' = $false
        'Intel Quick Sync' = $false
        'NVIDIA CUVID' = $false
    }

    if ($null -eq $Stage) { throw "$Context is missing." }
    foreach ($property in @('samples', 'total_ms', 'mean_ms', 'max_ms')) {
        if ($Stage.PSObject.Properties.Name -notcontains $property) { throw "$Context omitted $property." }
    }
    if (-not (Test-HardwareTransferUnsignedInteger $Stage.samples)) { throw "$Context has an invalid samples value." }
    foreach ($property in @('total_ms', 'mean_ms', 'max_ms')) {
        if (-not (Test-HardwareTransferFiniteNonnegativeNumber $Stage.$property)) {
            throw "$Context has an invalid $property value."
        }
    }

    if ($DecoderBackends.Count -eq 0) { throw "$Context has no observed decoder backend identity." }
    $requiresTransfer = $false
    foreach ($backendValue in $DecoderBackends) {
        $backend = [string]$backendValue
        if ([string]::IsNullOrWhiteSpace($backend) -or -not $knownDecodeBackendTransferRequirements.ContainsKey($backend)) {
            throw "$Context has an unknown or blank decoder backend identity."
        }
        $requiresTransfer = $requiresTransfer -or $knownDecodeBackendTransferRequirements[$backend]
    }

    $samples = [uint64]$Stage.samples
    $total = [double]$Stage.total_ms
    $mean = [double]$Stage.mean_ms
    $max = [double]$Stage.max_ms
    if ($samples -eq 0) {
        if ($total -ne 0 -or $mean -ne 0 -or $max -ne 0) { throw "$Context reported durations without samples." }
        if ($requiresTransfer) { throw "$Context reported zero samples despite an observed transfer-required decoder backend." }
        return
    }
    if ($total -lt $max) { throw "$Context has total below max." }
    if ($max -lt $mean) { throw "$Context has max below mean." }
    $expectedMean = $total / [double]$samples
    $meanTolerance = [Math]::Max(0.000001, [Math]::Abs($expectedMean) * 0.000000001)
    if ([Math]::Abs($mean - $expectedMean) -gt $meanTolerance) { throw "$Context has an inconsistent mean." }
}
