[CmdletBinding()]
param([string]$ReportPath)

$ErrorActionPreference = 'Stop'
$repoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$cargo = 'C:\Users\The Kraken\.cargo\bin\cargo.exe'
$ffmpegRoot = Join-Path $repoRoot '.deps\ffmpeg-project-8.1'
$libclangRoot = Join-Path $repoRoot '.deps\libclang-bindgen'
$artifactRoot = [IO.Path]::GetFullPath((Join-Path $repoRoot 'artifacts\phase1-multisource'))
if ([string]::IsNullOrWhiteSpace($ReportPath)) { $ReportPath = Join-Path $artifactRoot 'phase1-generation-stress.json' }
$resolvedReport = if ([IO.Path]::IsPathRooted($ReportPath)) { [IO.Path]::GetFullPath($ReportPath) } else { [IO.Path]::GetFullPath((Join-Path $repoRoot $ReportPath)) }
if ([IO.Path]::GetDirectoryName($resolvedReport) -ine $artifactRoot -or [IO.Path]::GetExtension($resolvedReport) -ine '.json') {
    throw "Report output must be JSON directly inside $artifactRoot"
}

function Assert-UnsignedInteger($Object, [string]$Name) {
    $property = $Object.PSObject.Properties[$Name]
    $value = if ($null -ne $property) { $property.Value } else { $null }
    if (($value -isnot [int] -and $value -isnot [long] -and $value -isnot [uint64]) -or $value -lt 0) {
        throw "Generation stress report omitted or invalidated unsigned integer $Name."
    }
}

function Normalize-ExtendedPath([string]$Path) {
    if ($Path.StartsWith('\\?\')) { return $Path.Substring(4) }
    return $Path
}

$saved = @{}
foreach ($name in @('PATH', 'FFMPEG_DIR', 'LIBCLANG_PATH', 'MAELSTROM_TEST_MEDIA', 'MAELSTROM_TEST_MEDIA_SECOND', 'MAELSTROM_TEST_MEDIA_THIRD', 'MAELSTROM_TEST_MEDIA_FOURTH', 'MAELSTROM_PHASE1_GENERATION_STRESS_REPORT')) {
    $saved[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
}
$lock = [Threading.Mutex]::new($false, 'Local\MaelstromRustPhase1SustainedFixtureLock')
$held = $false
Push-Location -LiteralPath $repoRoot
try {
    # The existing gate generates and verifies all four dynamic Full-1080p fixtures.
    & (Join-Path $PSScriptRoot 'Run-Phase1Multisource.ps1')
    if ($LASTEXITCODE -ne 0) { throw 'Four-source fixture gate failed before generation stress.' }
    if (-not $lock.WaitOne(0)) { throw 'Another Phase 1 fixture run owns the exclusive artifact lock.' }
    $held = $true
    $videos = @(0, 60, 120, 180) | ForEach-Object { Join-Path $artifactRoot ('source-hue-{0:d3}.mp4' -f $_) }
    $env:FFMPEG_DIR = $ffmpegRoot
    $env:LIBCLANG_PATH = $libclangRoot
    $env:PATH = (Join-Path $ffmpegRoot 'bin') + [IO.Path]::PathSeparator + $libclangRoot + [IO.Path]::PathSeparator + $saved['PATH']
    $env:MAELSTROM_TEST_MEDIA = $videos[0]
    $env:MAELSTROM_TEST_MEDIA_SECOND = $videos[1]
    $env:MAELSTROM_TEST_MEDIA_THIRD = $videos[2]
    $env:MAELSTROM_TEST_MEDIA_FOURTH = $videos[3]
    $env:MAELSTROM_PHASE1_GENERATION_STRESS_REPORT = $resolvedReport
    Remove-Item -LiteralPath $resolvedReport -Force -ErrorAction SilentlyContinue
    & $cargo test -p nle-app --release tests::supplied_media_layer_toggle_backward_scrub_stress -- --ignored --exact --test-threads=1
    if ($LASTEXITCODE -ne 0) { throw 'Real-media layer-toggle/backward-scrub gate failed.' }
    if (-not (Test-Path -LiteralPath $resolvedReport -PathType Leaf)) { throw 'Generation stress report was not written.' }
    $report = Get-Content -LiteralPath $resolvedReport -Raw | ConvertFrom-Json
    foreach ($name in @('schema_version', 'source_count', 'cycles', 'resource_checkpoint_count')) {
        Assert-UnsignedInteger $report $name
    }
    foreach ($name in @('active_sessions', 'live_source_groups', 'live_lane_actors', 'retiring_lane_actors')) {
        Assert-UnsignedInteger $report.post_drop $name
        if ($report.post_drop.$name -ne 0) { throw "Generation stress retained $name after teardown." }
    }
    if ($report.schema_version -ne 1 -or $report.status -ne 'passed' -or $report.source_count -ne 4 -or $report.cycles -ne 32 -or
        $report.resources_valid -isnot [bool] -or -not $report.resources_valid -or $report.resource_checkpoint_count -ne 96) {
        throw 'Generation stress report failed its workload/resource contract.'
    }
    if ((@($report.output_size) -join ',') -ne '640,360') { throw 'Generation stress output size must be 640x360.' }
    $expectedOperations = @{ forward_submits = 32; backward_submits = 32; disable_operations = 33; reenable_operations = 33; barrier_supersessions = 1 }
    foreach ($name in $expectedOperations.Keys) {
        Assert-UnsignedInteger $report.operations $name
        if ($report.operations.$name -ne $expectedOperations[$name]) { throw "Generation stress did not exercise the expected $name count." }
    }
    Assert-UnsignedInteger $report.stale_rejection 'barrier_request_id'
    if ($report.stale_rejection.barrier_request_id -eq 0) { throw 'Generation stress omitted the blocked request ID.' }
    foreach ($name in @('barrier_blocked', 'captured_real_frame_replayed_after_generation', 'captured_real_frame_rejected', 'matching_generation_control_presented')) {
        if ($report.stale_rejection.$name -isnot [bool] -or -not $report.stale_rejection.$name) { throw "Generation stress did not prove $name." }
    }
    Assert-UnsignedInteger $report.stale_rejection 'control_generation'
    $capturedIdentity = @($report.stale_rejection.captured_real_frame_identity)
    if ($capturedIdentity.Count -ne 2 -or $capturedIdentity[0] -le 0 -or $capturedIdentity[1] -le 0 -or
        $report.stale_rejection.control_generation -le $capturedIdentity[0]) {
        throw 'Generation stress did not isolate rejection of an older generation.'
    }
    Assert-UnsignedInteger $report.runtime_diagnostics_delta 'monitor_errors'
    Assert-UnsignedInteger $report.runtime_diagnostics_delta 'monitor_dropped_frames'
    if ($report.runtime_diagnostics_delta.monitor_errors -ne 0 -or $report.runtime_diagnostics_delta.monitor_dropped_frames -lt 33) {
        throw 'Generation stress must reject all 33 replayed stale frames with zero current decoder errors.'
    }
    $cycles = @($report.per_cycle)
    if ($cycles.Count -ne 32) { throw 'Generation stress omitted per-cycle evidence.' }
    for ($cycleIndex = 0; $cycleIndex -lt 32; $cycleIndex++) {
        $cycle = $cycles[$cycleIndex]
        foreach ($name in @('cycle', 'toggled_layer', 'forward_playhead_tick', 'backward_playhead_tick')) { Assert-UnsignedInteger $cycle $name }
        if ($cycle.cycle -ne $cycleIndex -or $cycle.toggled_layer -ne (($cycleIndex + 3) % 4) -or
            $cycle.forward_playhead_tick - $cycle.backward_playhead_tick -ne 166667) {
            throw "Generation stress cycle $cycleIndex did not follow the rotating-layer/backward-scrub workload."
        }
        foreach ($name in @('disabled_frame_cleared', 'unaffected_layers_retained', 'captured_real_frame_replay_rejected')) {
            if ($cycle.$name -isnot [bool] -or -not $cycle.$name) { throw "Generation stress cycle $cycleIndex did not prove $name." }
        }
        $forward = @($cycle.forward_identities)
        $disabled = @($cycle.disabled_identities)
        $latest = @($cycle.latest_identities)
        $applied = @($cycle.final_applied_identities)
        if ($forward.Count -ne 4 -or $disabled.Count -ne 3 -or $latest.Count -ne 4 -or $applied.Count -ne 4) { throw "Generation stress cycle $cycleIndex omitted identity evidence." }
        for ($layer = 0; $layer -lt 4; $layer++) {
            foreach ($identity in @($forward[$layer], $latest[$layer])) {
                foreach ($name in @('layer', 'generation', 'request_id', 'media_id', 'source_tick')) { Assert-UnsignedInteger $identity $name }
                if ($identity.layer -ne $layer -or $identity.media_id -ne ($layer + 1) -or $identity.generation -eq 0 -or $identity.request_id -eq 0) {
                    throw "Generation stress cycle $cycleIndex has an unrelated source identity."
                }
            }
            if ($forward[$layer].source_tick -ne $cycle.forward_playhead_tick -or
                $latest[$layer].source_tick -lt $cycle.backward_playhead_tick -or $latest[$layer].source_tick -gt ($cycle.backward_playhead_tick + 33334) -or
                $latest[$layer].request_id -le $forward[$layer].request_id) {
                throw "Generation stress cycle $cycleIndex retained an obsolete request or source time."
            }
            if ($layer -eq $cycle.toggled_layer -and $latest[$layer].generation -le $forward[$layer].generation) {
                throw "Generation stress cycle $cycleIndex did not invalidate the toggled generation."
            }
            if (@($applied[$layer]).Count -ne 2 -or $applied[$layer][0] -ne $latest[$layer].generation -or $applied[$layer][1] -ne $latest[$layer].request_id) {
                throw "Generation stress cycle $cycleIndex did not present the actual latest-generation event."
            }
        }
        $expectedMedia = @(1..4 | Where-Object { $_ -ne ($cycle.toggled_layer + 1) })
        for ($slot = 0; $slot -lt 3; $slot++) {
            foreach ($name in @('layer', 'generation', 'request_id', 'media_id', 'source_tick')) { Assert-UnsignedInteger $disabled[$slot] $name }
            if ($disabled[$slot].layer -ne $slot -or $disabled[$slot].media_id -ne $expectedMedia[$slot] -or $disabled[$slot].source_tick -ne $cycle.forward_playhead_tick) {
                throw "Generation stress cycle $cycleIndex did not remove the disabled source correctly."
            }
        }
    }
    $resources = $report.resources
    foreach ($name in @('frame_cache_capacity_bytes', 'current_frame_cache_bytes', 'peak_frame_cache_bytes_upper_bound',
        'active_sticky_sessions', 'peak_sticky_sessions', 'session_cap', 'active_foreground_sessions', 'foreground_session_cap',
        'active_background_sessions', 'background_session_cap', 'live_source_groups', 'source_group_cap', 'live_lane_actors', 'lane_actor_cap', 'retiring_lane_actors')) {
        Assert-UnsignedInteger $resources $name
    }
    if ($resources.frame_cache_capacity_bytes -le 0 -or $resources.current_frame_cache_bytes -gt $resources.frame_cache_capacity_bytes -or
        $resources.peak_frame_cache_bytes_upper_bound -gt $resources.frame_cache_capacity_bytes -or
        $resources.active_sticky_sessions -ne ($resources.active_foreground_sessions + $resources.active_background_sessions) -or
        $resources.session_cap -ne ($resources.foreground_session_cap + $resources.background_session_cap) -or
        $resources.active_sticky_sessions -gt $resources.session_cap -or $resources.peak_sticky_sessions -gt $resources.session_cap -or
        $resources.active_foreground_sessions -gt $resources.foreground_session_cap -or $resources.active_background_sessions -gt $resources.background_session_cap -or
        $resources.live_source_groups -gt $resources.source_group_cap -or
        ($resources.live_lane_actors + $resources.retiring_lane_actors) -gt $resources.lane_actor_cap) {
        throw 'Generation stress resource measurements exceeded their hard limits.'
    }
    $sources = @($report.sources)
    if ($sources.Count -ne 4) { throw 'Generation stress report omitted four source records.' }
    for ($index = 0; $index -lt 4; $index++) {
        $source = $sources[$index]
        Assert-UnsignedInteger $source 'size_bytes'
        $fixture = Get-Item -LiteralPath $videos[$index]
        if ($source.path -isnot [string] -or (Normalize-ExtendedPath $source.path) -ine $fixture.FullName -or
            $source.size_bytes -ne $fixture.Length -or $source.size_bytes -eq 0) {
            throw "Generation stress source provenance mismatch at index $index."
        }
    }
    $backends = @($report.observed_decoder_backends)
    if ($backends.Count -eq 0 -or @($backends | Where-Object { $_ -isnot [string] -or [string]::IsNullOrWhiteSpace($_) }).Count -ne 0) {
        throw 'Generation stress report omitted observed decoder backends.'
    }
    Write-Host "Phase 1 generation stress: PASS ($resolvedReport; 32 cycles, four independent sources)"
}
finally {
    foreach ($name in $saved.Keys) { [Environment]::SetEnvironmentVariable($name, $saved[$name], 'Process') }
    if ($held) { $lock.ReleaseMutex() }
    $lock.Dispose()
    Pop-Location
}
