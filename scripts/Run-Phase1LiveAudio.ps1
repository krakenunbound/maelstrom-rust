[CmdletBinding()]
param(
    [ValidateRange(2, 30)][int]$DurationSeconds = 5,
    [string]$ReportPath
)

$ErrorActionPreference = 'Stop'

function Restore-EnvironmentValue {
    param([Parameter(Mandatory = $true)][string]$Name, $Value)
    if ($null -eq $Value) { Remove-Item "Env:$Name" -ErrorAction SilentlyContinue }
    else { Set-Item "Env:$Name" $Value }
}

function Require-File([string]$Path, [string]$Description) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { throw "Missing ${Description}: $Path" }
}

function Normalize-ExtendedPath([string]$Path) {
    if ($Path.StartsWith('\\?\')) { return $Path.Substring(4) }
    return $Path
}

$repoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$cargo = 'C:\Users\The Kraken\.cargo\bin\cargo.exe'
$ffmpegRoot = Join-Path $repoRoot '.deps\ffmpeg-project-8.1'
$ffmpeg = Join-Path $ffmpegRoot 'bin\ffmpeg.exe'
$ffprobe = Join-Path $ffmpegRoot 'bin\ffprobe.exe'
$libclangRoot = Join-Path $repoRoot '.deps\libclang-bindgen'
$artifactRoot = [IO.Path]::GetFullPath((Join-Path $repoRoot 'artifacts\phase1-multisource'))
$fixtureRunner = Join-Path $PSScriptRoot 'Run-Phase1Multisource.ps1'
$audioPath = Join-Path $artifactRoot 'live-audio-60s.m4a'
if ([string]::IsNullOrWhiteSpace($ReportPath)) { $ReportPath = Join-Path $artifactRoot 'phase1-live-audio.json' }
$reportPath = if ([IO.Path]::IsPathRooted($ReportPath)) { [IO.Path]::GetFullPath($ReportPath) } else { [IO.Path]::GetFullPath((Join-Path $repoRoot $ReportPath)) }
if ([IO.Path]::GetDirectoryName($reportPath) -ine $artifactRoot -or [IO.Path]::GetExtension($reportPath) -ine '.json') { throw "Report output must be JSON directly inside $artifactRoot" }
Require-File $cargo 'the pinned Cargo executable'
Require-File $ffmpeg 'the pinned FFmpeg executable'
Require-File $ffprobe 'the pinned FFprobe executable'
Require-File (Join-Path $libclangRoot 'libclang.dll') 'the local libclang runtime'
Require-File $fixtureRunner 'the Phase 1 multisource fixture runner'
New-Item -ItemType Directory -Force -Path $artifactRoot | Out-Null

$hue = @(0, 60, 120, 180)
$videos = $hue | ForEach-Object { Join-Path $artifactRoot ("source-hue-{0:d3}.mp4" -f $_) }
# The established runner owns and validates the shared four-source fixture contract. Run it before
# taking this gate's lock so the two scripts never recursively contend for the same named mutex.
& $fixtureRunner
if ($LASTEXITCODE -ne 0) { throw 'Phase 1 multisource fixture gate failed before live audio.' }
foreach ($video in $videos) { Require-File $video 'a Full-1080p Phase 1 video fixture' }
$saved = @{}
foreach ($name in @('PATH','FFMPEG_DIR','LIBCLANG_PATH','MAELSTROM_TEST_MEDIA','MAELSTROM_TEST_MEDIA_SECOND','MAELSTROM_TEST_MEDIA_THIRD','MAELSTROM_TEST_MEDIA_FOURTH','MAELSTROM_PHASE1_AUDIO_MEDIA','MAELSTROM_PHASE1_LIVE_AUDIO_REPORT','MAELSTROM_PHASE1_LIVE_AUDIO_SECONDS')) { $saved[$name] = (Get-Item "Env:$name" -ErrorAction SilentlyContinue).Value }
$lock = [Threading.Mutex]::new($false, 'Local\MaelstromRustPhase1SustainedFixtureLock')
$held = $false
try {
    if (-not $lock.WaitOne(0)) { throw 'Another Phase 1 fixture run owns the exclusive artifact lock.' }
    $held = $true
    $env:FFMPEG_DIR = $ffmpegRoot; $env:LIBCLANG_PATH = $libclangRoot
    $env:PATH = (Join-Path $ffmpegRoot 'bin') + [IO.Path]::PathSeparator + $libclangRoot + [IO.Path]::PathSeparator + $saved['PATH']
    # Keep the four established video-only sources intact; only the dedicated audio fixture is regenerated.
    Remove-Item -LiteralPath $audioPath -Force -ErrorAction SilentlyContinue
    & $ffmpeg -hide_banner -loglevel error -y -f lavfi -i 'sine=frequency=440:sample_rate=48000' -t 60 -ac 2 -c:a aac -b:a 192k -movflags +faststart $audioPath
    if ($LASTEXITCODE -ne 0) { throw 'FFmpeg could not create the 60-second stereo AAC fixture.' }
    $probe = & $ffprobe -v error -select_streams a:0 -show_entries stream=codec_name,sample_rate,channels -of default=noprint_wrappers=1 $audioPath
    $durationProbe = & $ffprobe -v error -show_entries format=duration -of default=noprint_wrappers=1 $audioPath
    if ($LASTEXITCODE -ne 0 -or $probe -notcontains 'codec_name=aac' -or $probe -notcontains 'sample_rate=48000' -or $probe -notcontains 'channels=2' -or $durationProbe -notcontains 'duration=60.000000') { throw 'Audio fixture failed its AAC/48 kHz/stereo/60-second contract.' }
    $env:MAELSTROM_TEST_MEDIA = [IO.Path]::GetFullPath($videos[0]); $env:MAELSTROM_TEST_MEDIA_SECOND = [IO.Path]::GetFullPath($videos[1]); $env:MAELSTROM_TEST_MEDIA_THIRD = [IO.Path]::GetFullPath($videos[2]); $env:MAELSTROM_TEST_MEDIA_FOURTH = [IO.Path]::GetFullPath($videos[3])
    $env:MAELSTROM_PHASE1_AUDIO_MEDIA = [IO.Path]::GetFullPath($audioPath); $env:MAELSTROM_PHASE1_LIVE_AUDIO_REPORT = $reportPath; $env:MAELSTROM_PHASE1_LIVE_AUDIO_SECONDS = [string]$DurationSeconds
    Remove-Item -LiteralPath $reportPath -Force -ErrorAction SilentlyContinue
    & $cargo test -p nle-app --release tests::supplied_media_four_video_layers_preserve_live_audio_continuity -- --ignored --exact --test-threads=1
    if ($LASTEXITCODE -ne 0) { throw 'Phase 1 live-audio continuity gate failed.' }
    Require-File $reportPath 'the live-audio report'
    $report = Get-Content -LiteralPath $reportPath -Raw | ConvertFrom-Json
    $sourceTickDelta = [int64]$report.source_tick_end - [int64]$report.source_tick_start
    $videoProvenanceValid = @($report.video_sources).Count -eq 4
    for ($index = 0; $videoProvenanceValid -and $index -lt $videos.Count; $index++) {
        $videoItem = Get-Item -LiteralPath $videos[$index]
        $videoProvenanceValid = (Normalize-ExtendedPath ([string]$report.video_sources[$index].path)) -ieq $videoItem.FullName -and
            [int64]$report.video_sources[$index].size_bytes -eq $videoItem.Length
    }
    $audioItem = Get-Item -LiteralPath $audioPath
    $audioProvenanceValid = (Normalize-ExtendedPath ([string]$report.audio_source.path)) -ieq $audioItem.FullName -and
        [int64]$report.audio_source.size_bytes -eq $audioItem.Length
    $minimumPresentedFrames = [int64]$report.minimum_presentations_per_source * 4
    $submittedMonitorRequests = [int64]$report.monitor_request_count * 4
    $minimumMonitorRequests = [int64]$report.minimum_monitor_request_count * 4
    $minimumNonzeroMeterObservations = [Math]::Ceiling([int64]$report.meter_observation_count * 0.9)
    if ($report.schema_version -ne 2 -or $report.status -ne 'passed' -or
        $report.source_count -ne 4 -or -not $videoProvenanceValid -or -not $audioProvenanceValid -or
        $report.audio_target_count -ne 1 -or
        $report.max_meter -le 0 -or $report.final_meter -le 0 -or
        $report.meter_observation_count -le 0 -or
        $report.minimum_nonzero_meter_observations -ne $minimumNonzeroMeterObservations -or
        $report.nonzero_meter_observation_count -lt $minimumNonzeroMeterObservations -or
        $report.callback_sample_delta -le 0 -or
        $report.mix_sample_delta -le 0 -or $sourceTickDelta -le 0 -or
        $report.source_tick_delta -ne $sourceTickDelta -or
        $report.clock_drift_us -gt $report.clock_drift_limit_us -or
        $report.clock_drift_limit_us -ne 250000 -or
        $report.monitor_request_count -lt $report.minimum_monitor_request_count -or
        $report.slow_layer -ne 3 -or
        $report.slow_request_id -le 0 -or
        $report.requested_blocked_duration_ms -ne 750 -or
        $report.actual_blocked_duration_ms -lt $report.minimum_actual_blocked_duration_ms -or
        $report.minimum_actual_blocked_duration_ms -ne 750 -or
        $report.ready_source_presentations_during_block -lt $report.minimum_ready_source_presentations_during_block -or
        $report.minimum_ready_source_presentations_during_block -ne 2 -or
        $report.audio_tick_delta_during_block -lt $report.minimum_audio_tick_delta_during_block -or
        $report.minimum_audio_tick_delta_during_block -ne 500000 -or
        $report.slow_source_presentations_after_release -lt $report.minimum_slow_source_presentations_after_release -or
        $report.minimum_slow_source_presentations_after_release -ne 1 -or
        -not $report.slow_source_recovered -or
        @($report.source_exercise_counts).Count -ne 4 -or
        @($report.source_exercise_counts | Where-Object { $_ -lt $report.minimum_presentations_per_source }).Count -ne 0 -or
        $report.max_device_clock_stall_ms -gt $report.max_device_clock_stall_limit_ms -or
        $report.max_device_clock_stall_limit_ms -ne 250 -or
        $report.input_to_submit_us.p95 -gt $report.input_to_submit_p95_us_limit -or
        $report.input_to_submit_p95_us_limit -ne 1000 -or $report.transport_lost -or
        $null -ne $report.audio_error -or
        $report.audio_counter_delta.underrun_device_frames -ne 0 -or
        $report.audio_counter_delta.callback_lock_failures -ne 0 -or
        $report.audio_counter_delta.late_decoded_frames_discarded -ne 0 -or
        $report.runtime_diagnostics_delta.audio_underrun_frames -ne 0 -or
        $report.runtime_diagnostics_delta.audio_callback_lock_failures -ne 0 -or
        $report.runtime_diagnostics_delta.audio_late_discarded_frames -ne 0 -or
        $report.runtime_diagnostics_delta.monitor_requests -lt $minimumMonitorRequests -or
        $report.runtime_diagnostics_delta.monitor_requests -gt $submittedMonitorRequests -or
        $report.runtime_diagnostics_delta.monitor_requests % 4 -ne 0 -or
        $report.runtime_diagnostics_delta.monitor_completed_frames -lt $minimumPresentedFrames -or
        $report.runtime_diagnostics_delta.monitor_presented_frames -lt $minimumPresentedFrames -or
        $report.runtime_diagnostics_delta.monitor_errors -ne 0 -or
        $report.post_drop_active_sessions -ne 0) {
        throw "Live-audio report did not prove the required continuity contract: $($report | ConvertTo-Json -Compress -Depth 8)"
    }
    Write-Host "Phase 1 live audio: PASS ($reportPath; $DurationSeconds seconds)"
}
finally {
    foreach ($name in $saved.Keys) { Restore-EnvironmentValue $name $saved[$name] }
    if ($held) { $lock.ReleaseMutex() }; $lock.Dispose()
}
