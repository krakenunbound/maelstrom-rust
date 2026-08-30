#requires -Version 7.0
<# Headless report and process-ownership regression checks. Never launches the editor. #>
[CmdletBinding()]
param()
$ErrorActionPreference = 'Stop'
$tokens = $null
$parseErrors = $null
$runner = [Management.Automation.Language.Parser]::ParseFile((Join-Path $PSScriptRoot 'Run-Phase1Windowed.ps1'), [ref]$tokens, [ref]$parseErrors)
if ($parseErrors.Count) { throw ($parseErrors | Out-String) }
# Load only function definitions; executing the runner body would prepare/launch a workload.
foreach ($definition in $runner.EndBlock.Statements | Where-Object { $_ -is [Management.Automation.Language.FunctionDefinitionAst] }) {
    . ([scriptblock]::Create($definition.Extent.Text))
}

function New-ValidReport {
    $samples = @(0..47 | ForEach-Object {
        $index = $_
        $targets = @(0..3 | ForEach-Object { [ordered]@{ slot=$_; media_id=$_+1; clip_id=$_+1; generation=2; request_id=$index+1; requested_source_tick=1000000+$index*33334; output_size=@(1920,1080) } })
        $layers = @($targets | ForEach-Object { [ordered]@{ slot=$_.slot; media_id=$_.media_id; clip_id=$_.clip_id; generation=$_.generation; request_id=$_.request_id; source_tick=$_.requested_source_tick; output_size=@(1920,1080); backend=$null; upload_serial=$index*4+$_.slot+1; input_to_upload_ms=2.0 } })
        [ordered]@{ index=$index; warmup=($index -lt 8); playhead_tick=1000000+$index*33334; expected_playhead_tick=1000000+$index*33334; sequence_generation=7;
            input_to_ui_cpu_ms=0.2; full_cpu_frame_ms=0.5; input_to_surface_submission_ms=0.6; matching_layers_to_surface_ms=3.0;
            paint_serial=$index+1; paint_serial_before_input=$index; targets=$targets; layers=$layers }
    })
    $report = [ordered]@{ schema_version=1; run_id='headless-validator'; process_id=123; status='completed'; failure=$null; warmup_samples=8; measured_samples=40; cpu_budgets_passed=$true;
        configuration=[ordered]@{schema_version=1;run_id='headless-validator';source_paths=@('H:\one.mp4','H:\two.mp4','H:\three.mp4','H:\four.mp4');report_path='H:\case\app-report.json';adapter_class='DiscreteGpu'};
        environment=[ordered]@{renderer_device_type='DiscreteGpu';renderer_backend='Dx12';renderer_name='test-only renderer';requested_output_size=@(1920,1080);surface_size=@(1920,1080);decoder_backends=@('Software');
            cache_bytes=1024;cache_peak_bytes=2048;cache_cap_bytes=4096;active_sessions=4;peak_sessions=5;session_cap=8;runtime_diagnostics=@{monitor_errors=0}};
        samples=$samples;input_to_ui_cpu=@{samples=40;p50_ms=0.2;p95_ms=0.2;max_ms=0.2};full_cpu_frame=@{samples=40;p50_ms=0.5;p95_ms=0.5;max_ms=0.5};matching_layers_to_surface=@{samples=40;p50_ms=3.0;p95_ms=3.0;max_ms=3.0}}
    return $report | ConvertTo-Json -Depth 12 | ConvertFrom-Json
}
function Assert-FixtureReport($Report) {
    Assert-AppReport -Report $Report -RunId 'headless-validator' -ConfigPath 'H:\case\config.schema1.json' -Sources @('H:\one.mp4','H:\two.mp4','H:\three.mp4','H:\four.mp4') -Adapter DiscreteGpu -SourceCount 4 -ExpectedProcessId 123
}

Assert-FixtureReport (New-ValidReport)
$roundedReport = New-ValidReport
$roundedReport.samples[10].layers[3].source_tick = $roundedReport.samples[10].targets[3].requested_source_tick - 1
Assert-FixtureReport $roundedReport
$mutations = @(
    @{name='more than rounding preroll'; change={param($r) $r.samples[10].layers[3].source_tick=$r.samples[10].targets[3].requested_source_tick-2}},
    @{name='beyond target frame'; change={param($r) $r.samples[10].layers[3].source_tick=$r.samples[10].targets[3].requested_source_tick+33335}},
    @{name='wrong clip'; change={param($r) $r.samples[10].layers[3].clip_id=99}},
    @{name='stale generation'; change={param($r) $r.samples[10].layers[3].generation=1}},
    @{name='stale request'; change={param($r) $r.samples[10].layers[3].request_id=1}},
    @{name='missing layer'; change={param($r) $r.samples[10].layers=@($r.samples[10].layers[0..2])}},
    @{name='wrong media'; change={param($r) $r.samples[10].targets[3].media_id=1}},
    @{name='duplicate slot'; change={param($r) $r.samples[10].targets[3].slot=0}},
    @{name='downscaled input'; change={param($r) $r.samples[10].layers[3].output_size=@(640,360)}},
    @{name='stale paint'; change={param($r) $r.samples[10].paint_serial=$r.samples[10].paint_serial_before_input}},
    @{name='unmoved playhead'; change={param($r) $r.samples[10].playhead_tick=0}},
    @{name='bad sample order'; change={param($r) $r.samples[10].index=9}},
    @{name='bad warmup'; change={param($r) $r.samples[10].warmup=$true}},
    @{name='wrong timeline'; change={param($r) $r.samples[10].sequence_generation=8}},
    @{name='false percentile'; change={param($r) $r.input_to_ui_cpu.p95_ms=0.1}},
    @{name='nonfinite timing'; change={param($r) $r.samples[10].input_to_ui_cpu_ms=[double]::NaN}},
    @{name='numeric string'; change={param($r) $r.samples[10].input_to_ui_cpu_ms='0.2'}},
    @{name='boolean timing'; change={param($r) $r.samples[10].input_to_ui_cpu_ms=$true}},
    @{name='string identity'; change={param($r) $r.samples[10].layers[3].request_id='11'}},
    @{name='no observed backend'; change={param($r) $r.environment.decoder_backends=@()}},
    @{name='cache over cap'; change={param($r) $r.environment.cache_peak_bytes=4097}},
    @{name='session over cap'; change={param($r) $r.environment.peak_sessions=9}},
    @{name='wrong adapter'; change={param($r) $r.environment.renderer_device_type='IntegratedGpu'}},
    @{name='decoder fault'; change={param($r) $r.environment.runtime_diagnostics.monitor_errors=1}},
    @{name='false successful flag'; change={param($r) $r.cpu_budgets_passed='true'}}
)
foreach ($mutation in $mutations) {
    $report=New-ValidReport
    & $mutation.change $report
    $rejected=$false
    try { Assert-FixtureReport $report } catch { $rejected=$true }
    if (-not $rejected) { throw "Validator accepted $($mutation.name)." }
}

# A short hidden PowerShell helper verifies real Windows CIM dates, PID reuse protection,
# and exact owned-child cleanup. It does not load Maelstrom, Cargo, or any test executable.
$helper=$null
$identity=$null
try {
    $helper=Start-Process -FilePath (Join-Path $PSHOME 'pwsh.exe') -ArgumentList @('-NoProfile','-Command','Start-Sleep -Seconds 10') -WindowStyle Hidden -PassThru
    $identity=Get-ProcessIdentity $helper.Id
    if ($null -eq $identity -or -not (Test-ProcessIdentity $identity)) { throw 'Owned helper identity was not retained.' }
    $wrongIdentity=$identity.PSObject.Copy()
    $wrongIdentity.CreationDate=[DateTime]::UtcNow.AddDays(-1).ToString('o')
    Stop-OwnedProcessTree $wrongIdentity
    if (-not (Test-ProcessIdentity $identity)) { throw 'Cleanup killed a mismatched process identity.' }
    Stop-OwnedProcessTree $identity
    if (-not $helper.WaitForExit(5000) -or (Test-ProcessIdentity $identity)) { throw 'Owned helper did not exit.' }
} finally {
    if ($identity) { Stop-OwnedProcessTree $identity }
    elseif ($helper -and -not $helper.HasExited) { Stop-Process -Id $helper.Id -Force -ErrorAction SilentlyContinue }
    if ($helper) { $helper.Dispose() }
}
Write-Host "Windowed report validation: valid control + $($mutations.Count) rejected corruptions; Windows process identity/cleanup: PASS. No editor launched."
