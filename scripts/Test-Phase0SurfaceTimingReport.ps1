#requires -Version 7.0
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'Assert-Phase0SurfaceTimingReport.ps1')

function New-CumulativeStage([uint64]$Samples = 2) {
    if ($Samples -eq 0) { return [pscustomobject]@{ samples = 0; total_ms = 0.0; mean_ms = 0.0; max_ms = 0.0 } }
    [pscustomobject]@{ samples = $Samples; total_ms = [double]($Samples * 2); mean_ms = 2.0; max_ms = 3.0 }
}

function New-QuantileStage([uint64]$Samples = 2) {
    if ($Samples -eq 0) { return [pscustomobject]@{ samples = 0; p95_ms = 0.0; max_ms = 0.0 } }
    [pscustomobject]@{ samples = $Samples; p95_ms = 2.0; max_ms = 3.0 }
}

function New-SurfaceTimingReport([bool]$TimestampQuerySupported, [string[]]$DecoderBackends) {
    $decoder = [ordered]@{}
    foreach ($name in @('cache_lookup', 'demux_packet', 'decoder_calls', 'scaler', 'rgba_copy_letterbox', 'worker_request')) {
        $decoder[$name] = New-CumulativeStage
    }
    $decoder.named_decoder_reopen = New-CumulativeStage 0
    $decoder.hardware_transfer = if ($DecoderBackends -contains 'Windows D3D11VA') { New-CumulativeStage } else { New-CumulativeStage 0 }
    [pscustomobject]@{
        schema_version = 9
        samples = 120
        decoder_backends = $DecoderBackends
        observation_scope = [pscustomobject]@{
            surface_submission_observed = $true
            surface_present_call_cpu_observed = $true
            gpu_submission_completion_observed = $true
            physical_scanout_observed = $false
        }
        decoder_stage_timings = [pscustomobject]$decoder
        viewer_stage_timings = [pscustomobject]@{ upload_cpu = New-QuantileStage; compositor_encode_cpu = New-QuantileStage }
        audio_stage_timings = [pscustomobject]@{ output_callback_cpu = New-CumulativeStage; mix_render_cpu = New-CumulativeStage }
        gpu_stage_timings = [pscustomobject]@{
            timestamp_query_supported = $TimestampQuerySupported
            composite_pass_gpu = if ($TimestampQuerySupported) { New-QuantileStage } else { $null }
            submission_to_completion_elapsed = New-QuantileStage
        }
    }
}

function Copy-Report($Report) { return $Report | ConvertTo-Json -Depth 10 | ConvertFrom-Json }
function Assert-Accepted([scriptblock]$Action, [string]$Name) {
    try { & $Action } catch { throw "$Name should have been accepted: $($_.Exception.Message)" }
}
function Assert-Rejected([scriptblock]$Action, [string]$Name) {
    try { & $Action } catch { return }
    throw "$Name should have been rejected."
}

$supported = New-SurfaceTimingReport $true @('Windows D3D11VA')
$unsupported = New-SurfaceTimingReport $false @('Software')
Assert-Accepted { Assert-Phase0SurfaceTimingReport $supported 'supported timestamp-query report' } 'supported timestamp-query report'
Assert-Accepted { Assert-Phase0SurfaceTimingReport $unsupported 'unsupported timestamp-query report' } 'unsupported timestamp-query report'

$cases = @(
    @{ name = 'unsupported schema'; change = { param($r) $r.schema_version = 8 } },
    @{ name = 'fractional samples'; change = { param($r) $r.decoder_stage_timings.cache_lookup.samples = 1.5 } },
    @{ name = 'nonfinite duration'; change = { param($r) $r.audio_stage_timings.mix_render_cpu.max_ms = [double]::NaN } },
    @{ name = 'sample duration mismatch'; change = { param($r) $r.viewer_stage_timings.upload_cpu.samples = 0 } },
    @{ name = 'inconsistent cumulative mean'; change = { param($r) $r.decoder_stage_timings.decoder_calls.mean_ms = 1.0 } },
    @{ name = 'cumulative max below mean'; change = { param($r) $r.audio_stage_timings.output_callback_cpu.max_ms = 1.0 } },
    @{ name = 'quantile p95 above max'; change = { param($r) $r.gpu_stage_timings.submission_to_completion_elapsed.p95_ms = 4.0 } },
    @{ name = 'overstated scanout'; change = { param($r) $r.observation_scope.physical_scanout_observed = $true } },
    @{ name = 'missing scope boolean'; change = { param($r) $r.observation_scope.PSObject.Properties.Remove('surface_submission_observed') } },
    @{ name = 'string scope boolean'; change = { param($r) $r.observation_scope.gpu_submission_completion_observed = 'true' } },
    @{ name = 'scalar decoder backend'; change = { param($r) $r.decoder_backends = 'Windows D3D11VA' } },
    @{ name = 'unsupported timestamp composite'; change = { param($r) $r.gpu_stage_timings.timestamp_query_supported = $false; $r.gpu_stage_timings.composite_pass_gpu = New-QuantileStage } },
    @{ name = 'missing decoder stage'; change = { param($r) $r.decoder_stage_timings.PSObject.Properties.Remove('scaler') } },
    @{ name = 'missing decoder group'; change = { param($r) $r.PSObject.Properties.Remove('decoder_stage_timings') } },
    @{ name = 'missing viewer group'; change = { param($r) $r.PSObject.Properties.Remove('viewer_stage_timings') } },
    @{ name = 'missing audio group'; change = { param($r) $r.PSObject.Properties.Remove('audio_stage_timings') } },
    @{ name = 'missing GPU group'; change = { param($r) $r.PSObject.Properties.Remove('gpu_stage_timings') } },
    @{ name = 'transfer-required zero samples'; change = { param($r) $r.decoder_stage_timings.hardware_transfer = New-CumulativeStage 0 } }
)
foreach ($case in $cases) {
    $report = Copy-Report $supported
    & $case.change $report
    Assert-Rejected { Assert-Phase0SurfaceTimingReport $report $case.name } $case.name
}

foreach ($validator in @('package-windows.ps1', 'Run-Phase0CrossAdapterSurface.ps1')) {
    $text = Get-Content -LiteralPath (Join-Path $PSScriptRoot $validator) -Raw
    if ($text -notmatch 'Assert-Phase0SurfaceTimingReport\.ps1' -or
        $text -notmatch 'Assert-Phase0SurfaceTimingReport\s+-Report') {
        throw "$validator does not statically integrate the shared schema-9 timing validator."
    }
}

Write-Host 'Phase 0 surface timing report contract: PASS'
