[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateNotNullOrEmpty()]
    [string]$FfmpegRoot,
    [string]$ReportPath,
    [switch]$SkipFixtureValidation
)

$ErrorActionPreference = 'Stop'
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
if ([string]::IsNullOrWhiteSpace($ReportPath)) {
    $ReportPath = Join-Path $repoRoot 'artifacts\phase0-scenarios\phase0-scenarios.json'
}
function Test-AbsolutePath([string]$Path) {
    return [IO.Path]::IsPathRooted($Path) -and [string]::Equals([IO.Path]::GetFullPath($Path), $Path, [StringComparison]::OrdinalIgnoreCase)
}
function Enter-Phase0ScenarioMutex {
    $mutex = [Threading.Mutex]::new($false, 'Local\MaelstromPhase0ScenarioMatrix')
    try {
        try { $acquired = $mutex.WaitOne(0) }
        catch [Threading.AbandonedMutexException] { $acquired = $true }
        if (-not $acquired) { throw 'Another Phase 0 scenario matrix is already running.' }
        return $mutex
    } catch {
        $mutex.Dispose()
        throw
    }
}
if (-not (Test-AbsolutePath $FfmpegRoot)) { throw 'FFmpeg root must be an absolute path.' }
$ffmpegRootPath = (Resolve-Path -LiteralPath $FfmpegRoot).Path
$ffmpeg = Join-Path $ffmpegRootPath 'bin\ffmpeg.exe'
$ffprobe = Join-Path $ffmpegRootPath 'bin\ffprobe.exe'
if (-not (Test-Path -LiteralPath $ffmpeg -PathType Leaf) -or -not (Test-Path -LiteralPath $ffprobe -PathType Leaf)) {
    throw "Expected ffmpeg.exe and ffprobe.exe below $ffmpegRootPath\\bin."
}
$ffmpegVersion = & $ffmpeg -hide_banner -version 2>&1
if ($LASTEXITCODE -ne 0 -or $ffmpegVersion[0] -notmatch '^ffmpeg version n?8\.1(?:[.\s-]|$)') {
    throw "Phase 0 scenarios require the pinned FFmpeg 8.1 bundle: $ffmpegRootPath"
}

$artifactRoot = Join-Path $repoRoot 'artifacts\phase0-scenarios'
$resolvedArtifactRoot = [IO.Path]::GetFullPath($artifactRoot)
$resolvedReportPath = if ([IO.Path]::IsPathRooted($ReportPath)) {
    [IO.Path]::GetFullPath($ReportPath)
} else {
    [IO.Path]::GetFullPath((Join-Path $repoRoot $ReportPath))
}
if (-not [string]::Equals([IO.Path]::GetDirectoryName($resolvedReportPath), $resolvedArtifactRoot, [StringComparison]::OrdinalIgnoreCase)) {
    throw "Report output must be directly inside the ignored artifact directory: $resolvedArtifactRoot"
}
if ([IO.Path]::GetExtension($resolvedReportPath) -ne '.json') { throw 'Report output must be a JSON file.' }
New-Item -ItemType Directory -Force -Path $resolvedArtifactRoot | Out-Null

$fixtureRoot = Join-Path $repoRoot 'artifacts\media-fixtures'
$mediaPath = Join-Path $fixtureRoot 'bars-aac-2997.mp4'
$reorderedVfrPath = Join-Path $fixtureRoot 'vfr-reordered-mpeg2.ts'
$shiftedReorderedVfrPath = Join-Path $fixtureRoot 'vfr-reordered-shifted-mpeg4.mp4'
$shiftedFfv1VfrPath = Join-Path $fixtureRoot 'vfr-ffv1-shifted.mkv'
$proresVfrPath = Join-Path $fixtureRoot 'vfr-prores-10bit-shifted.mov'
$dnxhrVfrPath = Join-Path $fixtureRoot 'vfr-dnxhr-10bit-shifted.mov'
$av1VfrPath = Join-Path $fixtureRoot 'vfr-av1-aom-shifted.mkv'
$libclangRoot = if ([string]::IsNullOrWhiteSpace($env:LIBCLANG_PATH)) {
    Join-Path $repoRoot '.deps\libclang-bindgen'
} else {
    [IO.Path]::GetFullPath($env:LIBCLANG_PATH)
}
$libclang = Join-Path $libclangRoot 'libclang.dll'
if (-not (Test-Path -LiteralPath $libclang -PathType Leaf)) { throw "Missing local libclang required by native FFmpeg bindings: $libclang" }
$savedPath = $env:PATH
$savedFfmpeg = $env:FFMPEG_DIR
$savedLibclang = $env:LIBCLANG_PATH
$savedMedia = $env:MAELSTROM_PHASE0_MEDIA
$savedReorderedVfrMedia = $env:MAELSTROM_REORDERED_VFR_TEST_MEDIA
$savedShiftedReorderedVfrMedia = $env:MAELSTROM_SHIFTED_REORDERED_VFR_TEST_MEDIA
$savedShiftedFfv1VfrMedia = $env:MAELSTROM_FFV1_VFR_TEST_MEDIA
$savedProresVfrMedia = $env:MAELSTROM_PRORES_VFR_TEST_MEDIA
$savedDnxhrVfrMedia = $env:MAELSTROM_DNXHR_VFR_TEST_MEDIA
$savedAv1VfrMedia = $env:MAELSTROM_AV1_VFR_TEST_MEDIA
$savedReport = $env:MAELSTROM_PHASE0_REPORT
$savedArtifactRoot = $env:MAELSTROM_PHASE0_ARTIFACT_ROOT
$repoLocationPushed = $false
$phase0Mutex = $null
try {
    $phase0Mutex = Enter-Phase0ScenarioMutex
    if (-not $SkipFixtureValidation) {
        & (Join-Path $PSScriptRoot 'Generate-MediaFixtures.ps1') -FfmpegRoot $ffmpegRootPath
        & (Join-Path $PSScriptRoot 'Test-MediaFixtures.ps1') -FfmpegRoot $ffmpegRootPath -ArtifactRoot $fixtureRoot
        & (Join-Path $PSScriptRoot 'Prepare-Av1VfrFixture.ps1')
    }
    if (-not (Test-Path -LiteralPath $mediaPath -PathType Leaf)) { throw "Missing generated Phase 0 media fixture: $mediaPath" }
    if (-not (Test-Path -LiteralPath $reorderedVfrPath -PathType Leaf)) { throw "Missing generated reordered VFR fixture: $reorderedVfrPath" }
    $reorderedVfrPath = (Resolve-Path -LiteralPath $reorderedVfrPath).Path
    $shiftedReorderedVfrPath = (Resolve-Path -LiteralPath $shiftedReorderedVfrPath -ErrorAction Stop).Path
    $shiftedFfv1VfrPath = (Resolve-Path -LiteralPath $shiftedFfv1VfrPath -ErrorAction Stop).Path
    $proresVfrPath = (Resolve-Path -LiteralPath $proresVfrPath -ErrorAction Stop).Path
    $dnxhrVfrPath = (Resolve-Path -LiteralPath $dnxhrVfrPath -ErrorAction Stop).Path
    $av1VfrPath = (Resolve-Path -LiteralPath $av1VfrPath -ErrorAction Stop).Path

    Remove-Item -LiteralPath $resolvedReportPath -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath (Join-Path $resolvedArtifactRoot 'phase0-cancelled.mp4') -Force -ErrorAction SilentlyContinue
    $env:FFMPEG_DIR = $ffmpegRootPath
    $env:LIBCLANG_PATH = $libclangRoot
    $env:PATH = (Join-Path $ffmpegRootPath 'bin') + [IO.Path]::PathSeparator + $savedPath
    $env:MAELSTROM_PHASE0_MEDIA = $mediaPath
    $env:MAELSTROM_REORDERED_VFR_TEST_MEDIA = $reorderedVfrPath
    $env:MAELSTROM_SHIFTED_REORDERED_VFR_TEST_MEDIA = $shiftedReorderedVfrPath
    $env:MAELSTROM_FFV1_VFR_TEST_MEDIA = $shiftedFfv1VfrPath
    $env:MAELSTROM_PRORES_VFR_TEST_MEDIA = $proresVfrPath
    $env:MAELSTROM_DNXHR_VFR_TEST_MEDIA = $dnxhrVfrPath
    $env:MAELSTROM_AV1_VFR_TEST_MEDIA = $av1VfrPath
    $env:MAELSTROM_PHASE0_REPORT = $resolvedReportPath
    $env:MAELSTROM_PHASE0_ARTIFACT_ROOT = $resolvedArtifactRoot
    Push-Location -LiteralPath $repoRoot
    $repoLocationPushed = $true
    $cargoExecutable = (Get-Command cargo.exe -CommandType Application -ErrorAction Stop).Source
    if (-not (Test-AbsolutePath $cargoExecutable)) { throw 'Cargo did not resolve to an absolute executable path.' }
    & $cargoExecutable test -p nle-waveform tests::supplied_reordered_vfr_fixture_publishes_local_presentation_timestamps -- --exact --test-threads=1
    if ($LASTEXITCODE -ne 0) { throw "Focused reordered VFR waveform test failed with exit code $LASTEXITCODE." }
    & $cargoExecutable test -p nle-waveform tests::supplied_shifted_reordered_vfr_fixture_publishes_local_presentation_timestamps -- --exact --test-threads=1
    if ($LASTEXITCODE -ne 0) { throw "Focused shifted/reordered VFR waveform test failed with exit code $LASTEXITCODE." }
    & $cargoExecutable test -p nle-decode tests::supplied_reordered_vfr_fixture_reports_exact_local_frame_boundaries -- --exact --test-threads=1
    if ($LASTEXITCODE -ne 0) { throw "Focused reordered VFR decode test failed with exit code $LASTEXITCODE." }
    & $cargoExecutable test -p nle-app tests::supplied_reordered_vfr_fixture_routes_preview_to_local_presentation_boundaries -- --exact --test-threads=1
    if ($LASTEXITCODE -ne 0) { throw "Focused reordered VFR app test failed with exit code $LASTEXITCODE." }
    & $cargoExecutable test -p nle-decode --release scrub_seek_real_codec_vfr_generated_ -- --test-threads=1
    if ($LASTEXITCODE -ne 0) { throw "Generated 10-bit codec decode regressions failed with exit code $LASTEXITCODE." }
    & $cargoExecutable test -p nle-decode --release scrub_seek_tests::scrub_seek_real_codec_vfr_generated_shifted_reordered_mpeg4_matches_cli_reference -- --exact --test-threads=1
    if ($LASTEXITCODE -ne 0) { throw "Shifted/reordered MPEG-4 decode regression failed with exit code $LASTEXITCODE." }
    & $cargoExecutable test -p nle-decode --release scrub_seek_tests::generated_shifted_ffv1_vfr_scrub_matches_independent_cli_reference -- --exact --test-threads=1
    if ($LASTEXITCODE -ne 0) { throw "Shifted FFV1 VFR decode regression failed with exit code $LASTEXITCODE." }
    & $cargoExecutable test -p nle-app --release tests::supplied_shifted_vfr_fixtures_route_preview_to_local_boundaries -- --exact --test-threads=1
    if ($LASTEXITCODE -ne 0) { throw "Shifted VFR app regression failed with exit code $LASTEXITCODE." }
    & $cargoExecutable test -p nle-app --release tests::supplied_shifted_ffv1_analysis_normalizes_clip_and_strip_duration -- --exact --test-threads=1
    if ($LASTEXITCODE -ne 0) { throw "Shifted FFV1 media-analysis duration regression failed with exit code $LASTEXITCODE." }
    & $cargoExecutable test -p nle-app --release tests::supplied_video_reopens_with_cached_proxy_and_persists_validated_preference -- --exact --test-threads=1
    if ($LASTEXITCODE -ne 0) { throw "Cached proxy project-reopen regression failed with exit code $LASTEXITCODE." }
    & $cargoExecutable test -p nle-export --release vfr_export_tests -- --test-threads=1
    if ($LASTEXITCODE -ne 0) { throw "Shifted VFR export source-identity regressions failed with exit code $LASTEXITCODE." }
    & $cargoExecutable test -p nle-export --release tests::real_ffmpeg_export_cadence_retains_exact_rational_time_base -- --exact --test-threads=1
    if ($LASTEXITCODE -ne 0) { throw "Exact rational export cadence regression failed with exit code $LASTEXITCODE." }
    & $cargoExecutable test -p nle-app --release tests::phase0_scenario_matrix -- --ignored --exact --test-threads=1
    $testExitCode = $LASTEXITCODE

    if (-not (Test-Path -LiteralPath $resolvedReportPath -PathType Leaf)) {
        throw "Phase 0 scenario matrix exited with code $testExitCode without writing its report."
    }
    $report = Get-Content -LiteralPath $resolvedReportPath -Raw | ConvertFrom-Json
    if ($report.schema_version -ne 4 -or @('passed', 'failed') -notcontains $report.status -or [int]$report.scenario_count -ne 7 -or @($report.scenarios).Count -ne 7) {
        throw 'Phase 0 scenario report has an unexpected schema, status, or scenario count.'
    }
    foreach ($scenario in @($report.scenarios)) {
        if ([string]::IsNullOrWhiteSpace($scenario.name) -or [int]$scenario.iterations -lt 1 -or [double]$scenario.elapsed_ms -lt 0 -or
            -not ($scenario.passed -is [bool]) -or [string]::IsNullOrWhiteSpace($scenario.evidence)) {
            throw "Invalid scenario evidence in report: $($scenario.name)"
        }
    }
    $failedScenarios = @($report.scenarios | Where-Object { -not $_.passed })
    if ($testExitCode -ne 0 -or $report.status -ne 'passed' -or $failedScenarios.Count -ne 0) {
        $failedEvidence = @($failedScenarios | ForEach-Object { "$($_.name): $($_.evidence)" }) -join '; '
        throw "Phase 0 scenario matrix failed with exit code $testExitCode; preserved report: $resolvedReportPath. $failedEvidence"
    }
    $rapidSwitching = @($report.scenarios | Where-Object { $_.name -eq 'rapid_editor_state_switching' })
    if ($rapidSwitching.Count -ne 1 -or [int]$rapidSwitching[0].iterations -ne 8 -or $rapidSwitching[0].decoder_backend -ne 'Software' -or $null -ne $rapidSwitching[0].encoder_backend) {
        throw 'Phase 0 rapid editor-state switching scenario is missing its exact iteration/backend contract.'
    }
    $rapidEvidence = [string]$rapidSwitching[0].evidence
    # Keep this anchored and ordered so duplicate, contradictory, or unknown evidence is rejected.
    $rapidPattern = '\Aswitch_count=(?<switch_count>\d+) in_flight_cancellation_suppressions=(?<in_flight_cancellation_suppressions>\d+) stale_prior_generation_rejections=(?<stale_prior_generation_rejections>\d+) fresh_post_switch_presentations=(?<fresh_post_switch_presentations>\d+) generation_advances=(?<generation_advances>\d+) media_epoch_advances=(?<media_epoch_advances>\d+) monitor_errors=(?<monitor_errors>\d+) post_release_sessions=(?<post_release_sessions>\d+) post_release_groups=(?<post_release_groups>\d+) post_release_live_actors=(?<post_release_live_actors>\d+) post_release_retiring_actors=(?<post_release_retiring_actors>\d+)\z'
    $rapidMatch = [regex]::Match($rapidEvidence, $rapidPattern)
    if (-not $rapidMatch.Success) {
        throw "Phase 0 rapid editor-state switching evidence is missing required fields: $rapidEvidence"
    }
    foreach ($field in @('switch_count', 'in_flight_cancellation_suppressions', 'stale_prior_generation_rejections', 'fresh_post_switch_presentations', 'generation_advances', 'media_epoch_advances')) {
        if ([int64]$rapidMatch.Groups[$field].Value -ne 8) {
            throw "Phase 0 rapid editor-state switching evidence has an unexpected ${field}: $rapidEvidence"
        }
    }
    foreach ($field in @('monitor_errors', 'post_release_sessions', 'post_release_groups', 'post_release_live_actors', 'post_release_retiring_actors')) {
        if ([int64]$rapidMatch.Groups[$field].Value -ne 0) {
            throw "Phase 0 rapid editor-state switching evidence has a nonzero ${field}: $rapidEvidence"
        }
    }
    $memoryPressure = @($report.scenarios | Where-Object { $_.name -eq 'runtime_video_strip_cache_eviction' })
    if ($memoryPressure.Count -ne 1 -or [int]$memoryPressure[0].iterations -ne 5) {
        throw 'Phase 0 memory-pressure scenario is missing or has an unexpected iteration count.'
    }
    $evidence = [string]$memoryPressure[0].evidence
    if ($evidence -notmatch 'cumulative_bytes=(\d+) retained_bytes=(\d+) cap_bytes=(\d+) peak_live_bytes=(\d+)') {
        throw 'Phase 0 memory-pressure scenario does not report cumulative, retained, cap, and peak bytes.'
    }
    $cumulativeBytes = [int64]$Matches[1]
    $retainedBytes = [int64]$Matches[2]
    $capBytes = [int64]$Matches[3]
    $peakLiveBytes = [int64]$Matches[4]
    if ($cumulativeBytes -ne 367001600 -or $retainedBytes -ne 220200960 -or $capBytes -ne 268435456 -or $peakLiveBytes -ne 293601280 -or $retainedBytes -gt $capBytes -or $peakLiveBytes -le $capBytes) {
        throw "Phase 0 memory-pressure evidence is not the required bounded 70 MiB strip checkpoint: $evidence"
    }
    $decodedPressure = @($report.scenarios | Where-Object { $_.name -eq 'four_source_decoded_frame_cache_pressure' })
    if ($decodedPressure.Count -ne 1 -or [int]$decodedPressure[0].iterations -ne 4) {
        throw 'Phase 0 decoded-frame cache-pressure scenario is missing or has an unexpected iteration count.'
    }
    $decodedEvidence = [string]$decodedPressure[0].evidence
    $decodedPattern = '(?=.*\bsource_count=(?<source_count>\d+)\b)(?=.*\bframe_bytes=(?<frame_bytes>\d+)\b)(?=.*\bcap_bytes=(?<cap_bytes>\d+)\b)(?=.*\bcurrent_bytes=(?<current_bytes>\d+)\b)(?=.*\bpeak_bytes=(?<peak_bytes>\d+)\b)(?=.*\beviction_count=(?<eviction_count>\d+)\b)(?=.*\bpeak_sessions=(?<peak_sessions>\d+)\b)(?=.*\bsession_cap=(?<session_cap>\d+)\b)(?=.*\bsource_groups=(?<source_groups>\d+)\b)(?=.*\bsource_group_cap=(?<source_group_cap>\d+)\b)(?=.*\blane_actors=(?<lane_actors>\d+)\b)(?=.*\blane_actor_cap=(?<lane_actor_cap>\d+)\b)(?=.*\bpost_release_sessions=(?<post_release_sessions>\d+)\b)(?=.*\bpost_release_groups=(?<post_release_groups>\d+)\b)(?=.*\bpost_release_actors=(?<post_release_actors>\d+)\b)'
    $decodedMatch = [regex]::Match($decodedEvidence, $decodedPattern)
    if (-not $decodedMatch.Success) {
        throw "Phase 0 decoded-frame cache-pressure evidence is missing required fields: $decodedEvidence"
    }
    $decodedValues = @{}
    foreach ($field in @('source_count','frame_bytes','cap_bytes','current_bytes','peak_bytes','eviction_count','peak_sessions','session_cap','source_groups','source_group_cap','lane_actors','lane_actor_cap','post_release_sessions','post_release_groups','post_release_actors')) {
        $decodedValues[$field] = [int64]$decodedMatch.Groups[$field].Value
    }
    if ($decodedValues.source_count -ne 4 -or $decodedValues.frame_bytes -ne 57600 -or $decodedValues.cap_bytes -ne 172800 -or $decodedValues.current_bytes -gt $decodedValues.cap_bytes -or $decodedValues.peak_bytes -gt $decodedValues.cap_bytes -or $decodedValues.eviction_count -lt 1 -or $decodedValues.peak_sessions -lt $decodedValues.source_count -or $decodedValues.peak_sessions -gt $decodedValues.session_cap -or $decodedValues.source_groups -ne $decodedValues.source_count -or $decodedValues.source_groups -gt $decodedValues.source_group_cap -or $decodedValues.lane_actors -ne $decodedValues.source_count -or $decodedValues.lane_actors -gt $decodedValues.lane_actor_cap -or $decodedValues.post_release_sessions -ne 0 -or $decodedValues.post_release_groups -ne 0 -or $decodedValues.post_release_actors -ne 0) {
        throw "Phase 0 decoded-frame cache-pressure evidence is outside required bounds: $decodedEvidence"
    }
    $idlePressure = @($report.scenarios | Where-Object { $_.name -eq 'multi_source_pressure_and_idle_retirement' })
    if ($idlePressure.Count -ne 1 -or [int]$idlePressure[0].iterations -ne 16) {
        throw 'Phase 0 multi-source idle-retirement scenario is missing or has an unexpected iteration count.'
    }
    $idleEvidence = [string]$idlePressure[0].evidence
    $idlePattern = '(?=.*\bsource_count=(?<source_count>\d+)\b)(?=.*\bbatch_count=(?<batch_count>\d+)\b)(?=.*\blanes_per_batch=(?<lanes_per_batch>\d+)\b)(?=.*\bframe_bytes=(?<frame_bytes>\d+)\b)(?=.*\bcache_current_bytes=(?<cache_current_bytes>\d+)\b)(?=.*\bcache_peak_bytes=(?<cache_peak_bytes>\d+)\b)(?=.*\bcache_cap_bytes=(?<cache_cap_bytes>\d+)\b)(?=.*\bcache_eviction_count=(?<cache_eviction_count>\d+)\b)(?=.*\bpeak_sessions=(?<peak_sessions>\d+)\b)(?=.*\bsession_cap=(?<session_cap>\d+)\b)(?=.*\bpeak_source_groups=(?<peak_source_groups>\d+)\b)(?=.*\bsource_group_cap=(?<source_group_cap>\d+)\b)(?=.*\bpeak_lane_actors=(?<peak_lane_actors>\d+)\b)(?=.*\blane_actor_cap=(?<lane_actor_cap>\d+)\b)(?=.*\bidle_release_cycles=(?<idle_release_cycles>\d+)\b)(?=.*\bidle_lru_reclaims=(?<idle_lru_reclaims>\d+)\b)(?=.*\bretry_identity_preserved=true\b)(?=.*\bnewer_resident_owned=true\b)(?=.*\bidle_lru_final_ownership=sessions:(?<idle_lru_final_sessions>\d+) groups:(?<idle_lru_final_groups>\d+) live_actors:(?<idle_lru_final_live_actors>\d+) retiring_actors:(?<idle_lru_final_retiring_actors>\d+)\b)(?=.*\bfinal_sessions=(?<final_sessions>\d+)\b)(?=.*\bfinal_source_groups=(?<final_source_groups>\d+)\b)(?=.*\bfinal_live_lane_actors=(?<final_live_lane_actors>\d+)\b)(?=.*\bfinal_retiring_lane_actors=(?<final_retiring_lane_actors>\d+)\b)'
    $idleMatch = [regex]::Match($idleEvidence, $idlePattern)
    if (-not $idleMatch.Success) {
        throw "Phase 0 multi-source idle-retirement evidence is missing required fields: $idleEvidence"
    }
    $idleValues = @{}
    foreach ($field in @('source_count','batch_count','lanes_per_batch','frame_bytes','cache_current_bytes','cache_peak_bytes','cache_cap_bytes','cache_eviction_count','peak_sessions','session_cap','peak_source_groups','source_group_cap','peak_lane_actors','lane_actor_cap','idle_release_cycles','idle_lru_reclaims','idle_lru_final_sessions','idle_lru_final_groups','idle_lru_final_live_actors','idle_lru_final_retiring_actors','final_sessions','final_source_groups','final_live_lane_actors','final_retiring_lane_actors')) {
        $idleValues[$field] = [int64]$idleMatch.Groups[$field].Value
    }
    if ($idleValues.source_count -ne 12 -or $idleValues.batch_count -ne 3 -or $idleValues.lanes_per_batch -ne 4 -or $idleValues.frame_bytes -ne 57600 -or $idleValues.cache_cap_bytes -ne 172800 -or $idleValues.cache_current_bytes -ne $idleValues.cache_cap_bytes -or $idleValues.cache_peak_bytes -ne $idleValues.cache_cap_bytes -or $idleValues.cache_eviction_count -lt 9 -or $idleValues.peak_sessions -ne 4 -or $idleValues.session_cap -ne 4 -or $idleValues.peak_source_groups -ne 4 -or $idleValues.source_group_cap -ne 4 -or $idleValues.peak_lane_actors -ne 4 -or $idleValues.lane_actor_cap -lt 4 -or $idleValues.idle_release_cycles -ne 3 -or $idleValues.idle_lru_reclaims -ne 1 -or $idleValues.idle_lru_final_sessions -ne 0 -or $idleValues.idle_lru_final_groups -ne 0 -or $idleValues.idle_lru_final_live_actors -ne 0 -or $idleValues.idle_lru_final_retiring_actors -ne 0 -or $idleValues.final_sessions -ne 0 -or $idleValues.final_source_groups -ne 0 -or $idleValues.final_live_lane_actors -ne 0 -or $idleValues.final_retiring_lane_actors -ne 0) {
        throw "Phase 0 multi-source idle-retirement evidence is outside required bounds: $idleEvidence"
    }
    Write-Host "Phase 0 scenarios: PASS ($resolvedReportPath)"
}
finally {
    if ($repoLocationPushed) { Pop-Location }
    $env:PATH = $savedPath
    if ($null -eq $savedFfmpeg) { Remove-Item Env:FFMPEG_DIR -ErrorAction SilentlyContinue } else { $env:FFMPEG_DIR = $savedFfmpeg }
    if ($null -eq $savedLibclang) { Remove-Item Env:LIBCLANG_PATH -ErrorAction SilentlyContinue } else { $env:LIBCLANG_PATH = $savedLibclang }
    if ($null -eq $savedMedia) { Remove-Item Env:MAELSTROM_PHASE0_MEDIA -ErrorAction SilentlyContinue } else { $env:MAELSTROM_PHASE0_MEDIA = $savedMedia }
    if ($null -eq $savedReorderedVfrMedia) { Remove-Item Env:MAELSTROM_REORDERED_VFR_TEST_MEDIA -ErrorAction SilentlyContinue } else { $env:MAELSTROM_REORDERED_VFR_TEST_MEDIA = $savedReorderedVfrMedia }
    if ($null -eq $savedShiftedReorderedVfrMedia) { Remove-Item Env:MAELSTROM_SHIFTED_REORDERED_VFR_TEST_MEDIA -ErrorAction SilentlyContinue } else { $env:MAELSTROM_SHIFTED_REORDERED_VFR_TEST_MEDIA = $savedShiftedReorderedVfrMedia }
    if ($null -eq $savedShiftedFfv1VfrMedia) { Remove-Item Env:MAELSTROM_FFV1_VFR_TEST_MEDIA -ErrorAction SilentlyContinue } else { $env:MAELSTROM_FFV1_VFR_TEST_MEDIA = $savedShiftedFfv1VfrMedia }
    if ($null -eq $savedProresVfrMedia) { Remove-Item Env:MAELSTROM_PRORES_VFR_TEST_MEDIA -ErrorAction SilentlyContinue } else { $env:MAELSTROM_PRORES_VFR_TEST_MEDIA = $savedProresVfrMedia }
    if ($null -eq $savedDnxhrVfrMedia) { Remove-Item Env:MAELSTROM_DNXHR_VFR_TEST_MEDIA -ErrorAction SilentlyContinue } else { $env:MAELSTROM_DNXHR_VFR_TEST_MEDIA = $savedDnxhrVfrMedia }
    if ($null -eq $savedAv1VfrMedia) { Remove-Item Env:MAELSTROM_AV1_VFR_TEST_MEDIA -ErrorAction SilentlyContinue } else { $env:MAELSTROM_AV1_VFR_TEST_MEDIA = $savedAv1VfrMedia }
    if ($null -eq $savedReport) { Remove-Item Env:MAELSTROM_PHASE0_REPORT -ErrorAction SilentlyContinue } else { $env:MAELSTROM_PHASE0_REPORT = $savedReport }
    if ($null -eq $savedArtifactRoot) { Remove-Item Env:MAELSTROM_PHASE0_ARTIFACT_ROOT -ErrorAction SilentlyContinue } else { $env:MAELSTROM_PHASE0_ARTIFACT_ROOT = $savedArtifactRoot }
    if ($null -ne $phase0Mutex) { $phase0Mutex.ReleaseMutex(); $phase0Mutex.Dispose() }
}
