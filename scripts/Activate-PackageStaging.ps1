function Assert-PackageActivationSibling {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$Parent,
        [Parameter(Mandatory = $true)][string]$Label
    )

    $resolved = [IO.Path]::GetFullPath($Path)
    if (-not [string]::Equals([IO.Path]::GetDirectoryName($resolved), $Parent, [StringComparison]::OrdinalIgnoreCase)) {
        throw "${Label} must be a direct child of ${Parent}: $resolved"
    }
    return $resolved
}

function Invoke-PackageStagingActivation {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)][string]$StagingDirectory,
        [Parameter(Mandatory = $true)][string]$LiveDirectory,
        [scriptblock]$FailureInjector
    )

    $parent = [IO.Path]::GetFullPath((Split-Path -Parent $LiveDirectory))
    $live = Assert-PackageActivationSibling -Path $LiveDirectory -Parent $parent -Label 'Live package directory'
    $staging = Assert-PackageActivationSibling -Path $StagingDirectory -Parent $parent -Label 'Staging package directory'
    if ([string]::Equals($live, $staging, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'Staging and live package directories must differ.'
    }
    if (-not (Test-Path -LiteralPath $staging -PathType Container)) {
        throw "Staging package directory is missing: $staging"
    }
    if (Test-Path -LiteralPath $live -PathType Leaf) {
        throw "Live package path must be a directory when present: $live"
    }

    $backup = Join-Path $parent ('.{0}.rollback-{1}' -f (Split-Path -Leaf $live), [guid]::NewGuid().ToString('N'))
    $liveMoved = $false
    try {
        if (Test-Path -LiteralPath $live -PathType Container) {
            Move-Item -LiteralPath $live -Destination $backup -ErrorAction Stop
            $liveMoved = $true
        }
        if ($null -ne $FailureInjector) { & $FailureInjector }
        Move-Item -LiteralPath $staging -Destination $live -ErrorAction Stop
    } catch {
        $activationError = $_
        if ($liveMoved -and (Test-Path -LiteralPath $backup -PathType Container)) {
            if (Test-Path -LiteralPath $live -PathType Container) {
                if (-not (Test-Path -LiteralPath $staging)) {
                    Move-Item -LiteralPath $live -Destination $staging -ErrorAction Stop
                } else {
                    throw "Package activation failed and cannot restore the prior live package because both live and staging directories exist. Original failure: $($activationError.Exception.Message)"
                }
            }
            Move-Item -LiteralPath $backup -Destination $live -ErrorAction Stop
        }
        throw $activationError
    }
    if ($liveMoved -and (Test-Path -LiteralPath $backup -PathType Container)) {
        try {
            Remove-Item -LiteralPath $backup -Recurse -Force -ErrorAction Stop
        } catch {
            Write-Warning "New package is active, but the prior package backup remains at ${backup}: $($_.Exception.Message)"
        }
    }
    return Get-Item -LiteralPath $live
}
