param(
    [Parameter(Mandatory = $true)]
    [string]$ExecutablePath,
    [ValidateRange(5, 3600)]
    [int]$DurationSeconds = 600
)

$ErrorActionPreference = 'Stop'

function Write-AtomicUtf8File {
    param([Parameter(Mandatory = $true)][string]$Path, [Parameter(Mandatory = $true)][string]$Contents)
    $temporary = "$Path.tmp"
    [System.IO.File]::WriteAllText($temporary, $Contents, [System.Text.UTF8Encoding]::new($false))
    Move-Item -LiteralPath $temporary -Destination $Path -Force
}

function Restore-EnvironmentValue {
    param([Parameter(Mandatory = $true)][string]$Name, $Value)
    if ($null -eq $Value) {
        Remove-Item "Env:$Name" -ErrorAction SilentlyContinue
    } else {
        Set-Item "Env:$Name" $Value
    }
}

function Test-JsonIntegerValue {
    param($Value)
    return $Value -is [byte] -or $Value -is [sbyte] -or
        $Value -is [int16] -or $Value -is [uint16] -or
        $Value -is [int32] -or $Value -is [uint32] -or
        $Value -is [int64] -or $Value -is [uint64]
}

function Assert-JsonUnsignedIntegerProperty {
    param(
        [Parameter(Mandatory = $true)]$Object,
        [Parameter(Mandatory = $true)][string]$Name,
        [Parameter(Mandatory = $true)][string]$Context
    )
    $property = $Object.PSObject.Properties[$Name]
    if ($null -eq $property -or -not (Test-JsonIntegerValue $property.Value) -or $property.Value -lt 0) {
        throw "$Context omitted or invalidated unsigned integer $Name."
    }
}

function Test-JsonFiniteNumber {
    param($Value)
    $numeric = (Test-JsonIntegerValue $Value) -or $Value -is [single] -or
        $Value -is [double] -or $Value -is [decimal]
    if (-not $numeric) { return $false }
    $doubleValue = [double]$Value
    return -not [double]::IsNaN($doubleValue) -and -not [double]::IsInfinity($doubleValue)
}

if (-not [System.IO.Path]::IsPathRooted($ExecutablePath)) {
    throw 'ExecutablePath must be an absolute path to the packaged Maelstrom.exe.'
}
$resolvedExecutable = [System.IO.Path]::GetFullPath($ExecutablePath)
if (-not (Test-Path -LiteralPath $resolvedExecutable -PathType Leaf)) {
    throw "Packaged executable does not exist: $resolvedExecutable"
}
if ([System.IO.Path]::GetExtension($resolvedExecutable) -ine '.exe') {
    throw "ExecutablePath must name an .exe file: $resolvedExecutable"
}
$packageDirectory = [System.IO.Path]::GetDirectoryName($resolvedExecutable)
$ffmpeg = Join-Path $packageDirectory 'ffmpeg.exe'
$ffprobe = Join-Path $packageDirectory 'ffprobe.exe'
if (-not (Test-Path -LiteralPath $ffmpeg -PathType Leaf) -or -not (Test-Path -LiteralPath $ffprobe -PathType Leaf)) {
    throw 'The packaged executable directory must contain sibling ffmpeg.exe and ffprobe.exe.'
}

$repoRoot = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$artifactDirectory = [System.IO.Path]::GetFullPath((Join-Path $repoRoot 'artifacts\phase0-playback-soak'))
New-Item -ItemType Directory -Path $artifactDirectory -Force | Out-Null
$mediaPath = Join-Path $artifactDirectory 'deterministic-av-60s.mp4'
$appReportPath = Join-Path $artifactDirectory 'playback-soak-app-report.json'
$finalReportPath = Join-Path $artifactDirectory 'playback-soak-report.json'
Remove-Item -LiteralPath $appReportPath -Force -ErrorAction SilentlyContinue
Remove-Item -LiteralPath $finalReportPath -Force -ErrorAction SilentlyContinue

$savedPath = $env:PATH
$savedSmokeEditor = $env:MAELSTROM_SMOKE_EDITOR
$savedMediaPath = $env:MAELSTROM_MEDIA_ACCEPTANCE_PATH
$savedSoakReport = $env:MAELSTROM_PLAYBACK_SOAK_REPORT
$savedSoakSeconds = $env:MAELSTROM_PLAYBACK_SOAK_SECONDS
$process = $null
$workingSetSamples = [System.Collections.Generic.List[UInt64]]::new()
$maximumWorkingSetGrowthBytes = 1536MB

try {
    # Use the package's own FFmpeg binaries and DLL search path, never a system installation.
    $env:PATH = "$packageDirectory;C:\Windows\System32;C:\Windows"
    & $ffmpeg -hide_banner -version *> $null
    if ($LASTEXITCODE -ne 0) { throw 'Packaged ffmpeg.exe could not load with its sibling DLLs.' }
    & $ffprobe -hide_banner -version *> $null
    if ($LASTEXITCODE -ne 0) { throw 'Packaged ffprobe.exe could not load with its sibling DLLs.' }

    $savedErrorActionPreference = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    & $ffmpeg -hide_banner -y `
        -f lavfi -i 'testsrc2=size=320x180:rate=30' `
        -f lavfi -i 'sine=frequency=1000:sample_rate=48000' `
        -t 60 -c:v mpeg4 -q:v 8 -c:a aac -movflags +faststart $mediaPath *> $null
    $encodeExitCode = $LASTEXITCODE
    $ErrorActionPreference = $savedErrorActionPreference
    if ($encodeExitCode -ne 0 -or -not (Test-Path -LiteralPath $mediaPath -PathType Leaf)) {
        throw 'Packaged ffmpeg.exe could not create the deterministic 60-second A/V soak clip.'
    }

    $env:MAELSTROM_SMOKE_EDITOR = '1'
    $env:MAELSTROM_MEDIA_ACCEPTANCE_PATH = $mediaPath
    $env:MAELSTROM_PLAYBACK_SOAK_REPORT = $appReportPath
    $env:MAELSTROM_PLAYBACK_SOAK_SECONDS = [string]$DurationSeconds
    $process = Start-Process -FilePath $resolvedExecutable -WorkingDirectory $packageDirectory -WindowStyle Normal -PassThru
    $deadline = [DateTime]::UtcNow.AddSeconds($DurationSeconds + 120)
    while (-not (Test-Path -LiteralPath $appReportPath)) {
        if ([DateTime]::UtcNow -ge $deadline) {
            throw "Playback soak did not report within $DurationSeconds seconds plus its bounded startup allowance."
        }
        Start-Sleep -Seconds 1
        $process.Refresh()
        if ($process.HasExited) {
            throw "Packaged Maelstrom exited during playback soak with code $($process.ExitCode)."
        }
        # This deliberately samples only the exact launched GUI process. Child processes are
        # tracked solely for finally-cleanup; their memory is not represented as app RSS.
        $workingSetSamples.Add([uint64]$process.WorkingSet64)
    }
    $process.Refresh()
    if (-not $process.HasExited) {
        $workingSetSamples.Add([uint64]$process.WorkingSet64)
    }
    if ($workingSetSamples.Count -lt 3) {
        throw 'Playback soak completed before a warmed working-set baseline could be sampled.'
    }
    $warmBaselineBytes = $workingSetSamples[[Math]::Min(2, $workingSetSamples.Count - 1)]
    $peakWorkingSetBytes = ($workingSetSamples | Measure-Object -Maximum).Maximum
    $finalWorkingSetBytes = $workingSetSamples[$workingSetSamples.Count - 1]
    $peakGrowthBytes = [Math]::Max([int64]0, [int64]$peakWorkingSetBytes - [int64]$warmBaselineBytes)
    if ($peakGrowthBytes -gt $maximumWorkingSetGrowthBytes) {
        throw "Playback soak working-set growth exceeded the generous $maximumWorkingSetGrowthBytes-byte bound: $peakGrowthBytes bytes above warm baseline."
    }

    $appReport = Get-Content -LiteralPath $appReportPath -Raw | ConvertFrom-Json
    Assert-JsonUnsignedIntegerProperty $appReport 'schema_version' 'Playback soak report'
    Assert-JsonUnsignedIntegerProperty $appReport 'requested_duration_seconds' 'Playback soak report'
    Assert-JsonUnsignedIntegerProperty $appReport 'loop_count' 'Playback soak report'
    Assert-JsonUnsignedIntegerProperty $appReport 'monitor_cache_cap_bytes' 'Playback soak report'
    if (-not (Test-JsonFiniteNumber $appReport.actual_duration_seconds)) {
        throw 'Playback soak report omitted or invalidated finite actual_duration_seconds.'
    }
    $actualDurationSeconds = [double]$appReport.actual_duration_seconds
    $decoderBackends = $appReport.PSObject.Properties['observed_decoder_backends'].Value
    if ($appReport.schema_version -ne 3 -or
        $appReport.requested_duration_seconds -ne $DurationSeconds -or
        $actualDurationSeconds -lt $DurationSeconds -or
        $actualDurationSeconds -gt ($DurationSeconds + 2) -or
        $appReport.monitor_cache_cap_bytes -lt 1 -or
        $appReport.audio_transport_healthy_at_completion -isnot [bool] -or
        $appReport.audio_fault_observed -isnot [bool] -or
        $appReport.unexpected_playback_stop_observed -isnot [bool] -or
        $appReport.audio_transport_healthy_at_completion -ne $true -or
        $appReport.audio_fault_observed -ne $false -or
        $appReport.unexpected_playback_stop_observed -ne $false -or
        $appReport.selected_preview_quality -isnot [string] -or
        $appReport.resolved_preview_quality -isnot [string] -or
        $appReport.selected_preview_quality -ne 'Full' -or
        $appReport.resolved_preview_quality -ne 'Full' -or
        $decoderBackends -isnot [System.Array] -or
        $decoderBackends.Count -lt 1 -or
        @($decoderBackends | Where-Object { $_ -isnot [string] -or [string]::IsNullOrWhiteSpace($_) }).Count -ne 0) {
        throw 'Playback soak report omitted required full-quality runtime/environment evidence.'
    }
    $resources = $appReport.monitor_resources
    foreach ($property in @(
        'frame_cache_capacity_bytes', 'current_frame_cache_bytes', 'peak_frame_cache_bytes_upper_bound',
        'active_sticky_sessions', 'peak_sticky_sessions', 'session_cap',
        'active_foreground_sessions', 'foreground_session_cap',
        'active_background_sessions', 'background_session_cap'
    )) {
        Assert-JsonUnsignedIntegerProperty $resources $property 'Playback soak monitor resources'
    }
    if ($resources.frame_cache_capacity_bytes -lt 1 -or
        $resources.current_frame_cache_bytes -gt $resources.frame_cache_capacity_bytes -or
        $resources.current_frame_cache_bytes -gt $resources.peak_frame_cache_bytes_upper_bound -or
        $resources.peak_frame_cache_bytes_upper_bound -gt $resources.frame_cache_capacity_bytes -or
        $resources.active_sticky_sessions -gt $resources.session_cap -or
        $resources.active_sticky_sessions -gt $resources.peak_sticky_sessions -or
        $resources.peak_sticky_sessions -gt $resources.session_cap -or
        $resources.active_foreground_sessions -gt $resources.foreground_session_cap -or
        $resources.active_background_sessions -gt $resources.background_session_cap -or
        ($resources.active_foreground_sessions + $resources.active_background_sessions) -ne $resources.active_sticky_sessions -or
        ($resources.foreground_session_cap + $resources.background_session_cap) -ne $resources.session_cap) {
        throw "Playback soak reported monitor resources outside their aggregate bounds: $($resources | ConvertTo-Json -Compress)"
    }
    if ($resources.peak_frame_cache_bytes_upper_bound -lt 1 -or
        $resources.peak_sticky_sessions -lt 1) {
        throw "Playback soak did not exercise bounded monitor cache/session resources: $($resources | ConvertTo-Json -Compress)"
    }
    $delta = $appReport.runtime_diagnostics_delta
    foreach ($property in @(
        'monitor_requests', 'monitor_completed_frames', 'monitor_presented_frames',
        'monitor_dropped_frames', 'monitor_hold_events', 'monitor_late_frames', 'monitor_errors',
        'native_viewer_uploads', 'fallback_viewer_uploads', 'audio_underrun_frames',
        'audio_callback_lock_failures', 'audio_late_discarded_frames'
    )) {
        Assert-JsonUnsignedIntegerProperty $delta $property 'Playback soak runtime diagnostics'
    }
    foreach ($property in @('monitor_requests', 'monitor_completed_frames', 'monitor_presented_frames', 'native_viewer_uploads')) {
        if ($delta.$property -lt 1) {
            throw "Playback soak did not exercise $property."
        }
    }
    if ($delta.monitor_errors -ne 0) {
        throw "Playback soak observed $($delta.monitor_errors) monitor errors."
    }
    $maximumLateFrames = [Math]::Ceiling([double]$delta.monitor_requests * 0.02)
    if ($delta.monitor_late_frames -gt $maximumLateFrames -or $delta.fallback_viewer_uploads -ne 0 -or
        $delta.audio_underrun_frames -ne 0 -or $delta.audio_callback_lock_failures -ne 0 -or
        $delta.audio_late_discarded_frames -ne 0) {
        throw "Playback soak observed a late/fallback/audio failure delta: $($delta | ConvertTo-Json -Compress)"
    }
    $minimumNativeUploads = [Math]::Floor($actualDurationSeconds * 20)
    if ($delta.native_viewer_uploads -lt $minimumNativeUploads) {
        throw "Playback soak produced only $($delta.native_viewer_uploads) native uploads; expected at least $minimumNativeUploads for $actualDurationSeconds seconds."
    }
    $expectedMinimumLoops = [Math]::Max(0, [Math]::Floor($DurationSeconds / 60) - 1)
    if ($appReport.loop_count -lt $expectedMinimumLoops) {
        throw "Playback soak looped only $($appReport.loop_count) times; expected at least $expectedMinimumLoops for the requested wall duration."
    }
    $finalReport = [ordered]@{
        schema_version = 1
        executable_path = $resolvedExecutable
        executable_sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $resolvedExecutable).Hash
        duration_seconds_requested = $DurationSeconds
        app_report = $appReport
        working_set = [ordered]@{
            sample_interval_seconds = 1
            sample_count = $workingSetSamples.Count
            warm_baseline_bytes = $warmBaselineBytes
            peak_bytes = $peakWorkingSetBytes
            final_bytes = $finalWorkingSetBytes
            peak_growth_bytes = $peakGrowthBytes
            generous_peak_growth_bound_bytes = $maximumWorkingSetGrowthBytes
            scope = 'WorkingSet64 of the exact launched Maelstrom GUI process only; not total system, GPU, or child-process memory.'
        }
    }
    Write-AtomicUtf8File -Path $finalReportPath -Contents ($finalReport | ConvertTo-Json -Depth 8)
    Write-Host "Playback soak passed: $($appReport.actual_duration_seconds) s, $($appReport.loop_count) loops, peak working set $peakWorkingSetBytes bytes."
    Get-Item -LiteralPath $finalReportPath
} finally {
    if ($process) {
        try {
            # Exact PID tree only; never terminate a process by executable name.
            & "$env:SystemRoot\System32\taskkill.exe" /PID $process.Id /T /F *> $null
        } catch {
        }
        try { Wait-Process -Id $process.Id -ErrorAction SilentlyContinue } catch {}
    }
    Restore-EnvironmentValue -Name 'PATH' -Value $savedPath
    Restore-EnvironmentValue -Name 'MAELSTROM_SMOKE_EDITOR' -Value $savedSmokeEditor
    Restore-EnvironmentValue -Name 'MAELSTROM_MEDIA_ACCEPTANCE_PATH' -Value $savedMediaPath
    Restore-EnvironmentValue -Name 'MAELSTROM_PLAYBACK_SOAK_REPORT' -Value $savedSoakReport
    Restore-EnvironmentValue -Name 'MAELSTROM_PLAYBACK_SOAK_SECONDS' -Value $savedSoakSeconds
}
