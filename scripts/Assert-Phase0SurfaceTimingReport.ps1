. (Join-Path $PSScriptRoot 'Assert-HardwareTransferTiming.ps1')

function Test-Phase0SurfaceFiniteNonnegativeNumber {
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
    return -not [double]::IsNaN($number) -and -not [double]::IsInfinity($number) -and $number -ge 0
}

function Test-Phase0SurfaceUnsignedInteger {
    param($Value)

    if (-not (Test-Phase0SurfaceFiniteNonnegativeNumber $Value)) { return $false }
    try { return [double]$Value -eq [double][uint64]$Value } catch { return $false }
}

function Assert-Phase0SurfaceTimingCumulativeStage {
    param([Parameter(Mandatory = $true)]$Stage, [Parameter(Mandatory = $true)][string]$Context)

    if ($null -eq $Stage) { throw "$Context is missing." }
    foreach ($property in @('samples', 'total_ms', 'mean_ms', 'max_ms')) {
        if ($Stage.PSObject.Properties.Name -notcontains $property) { throw "$Context omitted $property." }
    }
    if (-not (Test-Phase0SurfaceUnsignedInteger $Stage.samples)) { throw "$Context has an invalid samples value." }
    foreach ($property in @('total_ms', 'mean_ms', 'max_ms')) {
        if (-not (Test-Phase0SurfaceFiniteNonnegativeNumber $Stage.$property)) { throw "$Context has an invalid $property value." }
    }

    $samples = [uint64]$Stage.samples
    $total = [double]$Stage.total_ms
    $mean = [double]$Stage.mean_ms
    $max = [double]$Stage.max_ms
    if ($samples -eq 0) {
        if ($total -ne 0 -or $mean -ne 0 -or $max -ne 0) { throw "$Context reported durations without samples." }
        return
    }
    if ($total -lt $max) { throw "$Context has total below max." }
    if ($max -lt $mean) { throw "$Context has max below mean." }
    $expectedMean = $total / [double]$samples
    $meanTolerance = [Math]::Max(0.000001, [Math]::Abs($expectedMean) * 0.000000001)
    if ([Math]::Abs($mean - $expectedMean) -gt $meanTolerance) { throw "$Context has an inconsistent mean." }
}

function Assert-Phase0SurfaceTimingQuantileStage {
    param([Parameter(Mandatory = $true)]$Stage, [Parameter(Mandatory = $true)][string]$Context)

    if ($null -eq $Stage) { throw "$Context is missing." }
    foreach ($property in @('samples', 'p95_ms', 'max_ms')) {
        if ($Stage.PSObject.Properties.Name -notcontains $property) { throw "$Context omitted $property." }
    }
    if (-not (Test-Phase0SurfaceUnsignedInteger $Stage.samples)) { throw "$Context has an invalid samples value." }
    foreach ($property in @('p95_ms', 'max_ms')) {
        if (-not (Test-Phase0SurfaceFiniteNonnegativeNumber $Stage.$property)) { throw "$Context has an invalid $property value." }
    }

    $samples = [uint64]$Stage.samples
    $p95 = [double]$Stage.p95_ms
    $max = [double]$Stage.max_ms
    if ($samples -eq 0) {
        if ($p95 -ne 0 -or $max -ne 0) { throw "$Context reported durations without samples." }
        return
    }
    if ($max -lt $p95) { throw "$Context has max below p95." }
}

function Assert-Phase0SurfaceTimingReport {
    param([Parameter(Mandatory = $true)]$Report, [Parameter(Mandatory = $true)][string]$Context)

    if ($null -eq $Report) { throw "$Context is missing." }
    foreach ($property in @('schema_version', 'samples', 'observation_scope', 'decoder_backends', 'decoder_stage_timings', 'viewer_stage_timings', 'audio_stage_timings', 'gpu_stage_timings')) {
        if ($Report.PSObject.Properties.Name -notcontains $property) { throw "$Context omitted $property." }
    }
    if (-not (Test-Phase0SurfaceUnsignedInteger $Report.schema_version) -or [uint64]$Report.schema_version -ne 9) {
        throw "$Context returned unsupported schema $($Report.schema_version)."
    }
    if (-not (Test-Phase0SurfaceUnsignedInteger $Report.samples)) { throw "$Context has an invalid samples value." }

    $scope = $Report.observation_scope
    if ($null -eq $scope) { throw "$Context is missing its observation scope." }
    foreach ($property in @('surface_submission_observed', 'surface_present_call_cpu_observed', 'gpu_submission_completion_observed', 'physical_scanout_observed')) {
        if ($scope.PSObject.Properties.Name -notcontains $property -or $scope.$property -isnot [bool]) {
            throw "$Context has an invalid observation-scope boolean $property."
        }
    }
    if ($scope.physical_scanout_observed) {
        throw "$Context has an invalid or overstated physical scanout observation."
    }
    if ($Report.decoder_backends -isnot [System.Collections.IList]) {
        throw "$Context decoder_backends must be a JSON array."
    }
    foreach ($group in @('decoder_stage_timings', 'viewer_stage_timings', 'audio_stage_timings', 'gpu_stage_timings')) {
        if ($null -eq $Report.$group) { throw "$Context is missing timing stage group $group." }
    }

    foreach ($stageName in @('cache_lookup', 'demux_packet', 'decoder_calls', 'scaler', 'rgba_copy_letterbox', 'worker_request', 'named_decoder_reopen')) {
        Assert-Phase0SurfaceTimingCumulativeStage -Stage $Report.decoder_stage_timings.$stageName -Context "$Context decoder stage $stageName"
    }
    Assert-HardwareTransferTiming -Stage $Report.decoder_stage_timings.hardware_transfer -DecoderBackends @($Report.decoder_backends) -Context "$Context decoder stage hardware_transfer"
    foreach ($stageName in @('upload_cpu', 'compositor_encode_cpu')) {
        Assert-Phase0SurfaceTimingQuantileStage -Stage $Report.viewer_stage_timings.$stageName -Context "$Context viewer stage $stageName"
    }
    foreach ($stageName in @('output_callback_cpu', 'mix_render_cpu')) {
        Assert-Phase0SurfaceTimingCumulativeStage -Stage $Report.audio_stage_timings.$stageName -Context "$Context audio stage $stageName"
    }

    $gpu = $Report.gpu_stage_timings
    foreach ($property in @('timestamp_query_supported', 'composite_pass_gpu', 'submission_to_completion_elapsed')) {
        if ($gpu.PSObject.Properties.Name -notcontains $property) { throw "$Context omitted GPU timing field $property." }
    }
    if ($gpu.timestamp_query_supported -isnot [bool]) { throw "$Context has a non-boolean GPU timestamp-query flag." }
    Assert-Phase0SurfaceTimingQuantileStage -Stage $gpu.submission_to_completion_elapsed -Context "$Context GPU submission-to-completion"
    if ($gpu.timestamp_query_supported) {
        Assert-Phase0SurfaceTimingQuantileStage -Stage $gpu.composite_pass_gpu -Context "$Context GPU compositor pass"
    } elseif ($null -ne $gpu.composite_pass_gpu) {
        throw "$Context serialized compositor GPU timing despite unavailable timestamp queries."
    }
}
