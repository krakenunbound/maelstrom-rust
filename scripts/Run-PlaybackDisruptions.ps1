param(
    [string]$LauncherPath = 'H:\Maelstrom Rust\Launch-Maelstrom-Editor.bat',
    [switch]$ValidateOnly
)

$ErrorActionPreference = 'Stop'

function Write-AtomicUtf8File {
    param([Parameter(Mandatory = $true)][string]$Path, [Parameter(Mandatory = $true)][string]$Contents)
    $temporary = "$Path.$PID.$([guid]::NewGuid().ToString('N')).tmp"
    try {
        [IO.File]::WriteAllText($temporary, $Contents, [Text.UTF8Encoding]::new($false))
        Move-Item -LiteralPath $temporary -Destination $Path -Force
    } finally {
        Remove-Item -LiteralPath $temporary -Force -ErrorAction SilentlyContinue
    }
}

function Remove-FileWithRetries {
    param([Parameter(Mandatory = $true)][string]$Path, [int]$Attempts = 3)
    for ($attempt = 1; $attempt -le $Attempts; $attempt++) {
        Remove-Item -LiteralPath $Path -Force -ErrorAction SilentlyContinue
        if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { return $true }
        if ($attempt -lt $Attempts) { Start-Sleep -Milliseconds 250 }
    }
    return $false
}

function Restore-EnvironmentValue {
    param([Parameter(Mandatory = $true)][string]$Name, $Value)
    if ($null -eq $Value) { Remove-Item "Env:$Name" -ErrorAction SilentlyContinue }
    else { Set-Item "Env:$Name" $Value }
}

function Test-JsonIntegerValue {
    param($Value)
    return $Value -is [byte] -or $Value -is [sbyte] -or $Value -is [int16] -or
        $Value -is [uint16] -or $Value -is [int32] -or $Value -is [uint32] -or
        $Value -is [int64] -or $Value -is [uint64]
}

function Assert-JsonUnsignedIntegerProperty {
    param([Parameter(Mandatory = $true)]$Object, [Parameter(Mandatory = $true)][string]$Name, [Parameter(Mandatory = $true)][string]$Context)
    $property = $Object.PSObject.Properties[$Name]
    if ($null -eq $property -or -not (Test-JsonIntegerValue $property.Value) -or $property.Value -lt 0) {
        throw "$Context omitted or invalidated unsigned integer $Name."
    }
}

function Test-JsonFiniteNumber {
    param($Value)
    $numeric = (Test-JsonIntegerValue $Value) -or $Value -is [single] -or $Value -is [double] -or $Value -is [decimal]
    if (-not $numeric) { return $false }
    $number = [double]$Value
    return -not [double]::IsNaN($number) -and -not [double]::IsInfinity($number)
}

function Find-OwnedPackagedEditorProcess {
    param([Parameter(Mandatory = $true)][int]$LauncherProcessId, [Parameter(Mandatory = $true)][string]$PackagedExecutable)
    # Batch can insert cmd.exe/start.exe. Only a descendant of this fresh launcher is evidence.
    $pending = [Collections.Generic.Queue[int]]::new()
    $pending.Enqueue($LauncherProcessId)
    while ($pending.Count -gt 0) {
        $parentId = $pending.Dequeue()
        foreach ($child in @(Get-CimInstance Win32_Process -Filter "ParentProcessId=$parentId" -ErrorAction SilentlyContinue)) {
            if ([string]::Equals([string]$child.ExecutablePath, $PackagedExecutable, [StringComparison]::OrdinalIgnoreCase)) {
                return Get-Process -Id ([int]$child.ProcessId) -ErrorAction SilentlyContinue
            }
            $pending.Enqueue([int]$child.ProcessId)
        }
    }
    return $null
}

$repoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$approvedLauncherPath = 'H:\Maelstrom Rust\Launch-Maelstrom-Editor.bat'
$artifactDirectory = [IO.Path]::GetFullPath((Join-Path $repoRoot 'artifacts\phase0-playback-disruptions'))
$mediaPath = Join-Path $artifactDirectory 'deterministic-full-av-60s.mp4'
$appReportPath = Join-Path $artifactDirectory 'playback-disruptions-app-report.json'
$finalReportPath = Join-Path $artifactDirectory 'playback-disruptions-report.json'
$exportPath = Join-Path $artifactDirectory 'playback-disruptions-cancelled.mp4'
$appReportTemporaryPath = "$appReportPath.tmp"
$finalReportTemporaryPath = "$finalReportPath.tmp"
$exportTemporaryPath = "$exportPath.part"

$savedPath = $env:PATH
$savedValues = @{}
foreach ($name in @(
    'MAELSTROM_SMOKE_EDITOR', 'MAELSTROM_MEDIA_ACCEPTANCE_PATH',
    'MAELSTROM_PLAYBACK_DISRUPTION_REPORT', 'MAELSTROM_PLAYBACK_DISRUPTION_EXPORT_PATH',
    'MAELSTROM_LAUNCHER_WAIT'
)) { $savedValues[$name] = (Get-Item "Env:$name" -ErrorAction SilentlyContinue).Value }

$stage = 'path_validation'
$resolvedLauncher = $null
$launcherSha256 = $null
$resolvedExecutable = $null
$executableSha256 = $null
$packageDirectory = $null
$ffmpeg = $null
$ffprobe = $null
$launcherProcess = $null
$appReport = $null

function Write-FailedPlaybackDisruptionReport {
    param([Parameter(Mandatory = $true)]$Failure)
    try {
        if ($null -eq $appReport -and (Test-Path -LiteralPath $appReportPath -PathType Leaf)) {
            try { $script:appReport = Get-Content -LiteralPath $appReportPath -Raw | ConvertFrom-Json } catch {}
        }
        $report = [ordered]@{
            schema_version = 1
            status = 'failed'
            failure = [ordered]@{ stage = $stage; error_type = $Failure.Exception.GetType().FullName; message = $Failure.Exception.Message }
            launcher_path = $resolvedLauncher
            launcher_sha256 = $launcherSha256
            executable_path = $resolvedExecutable
            executable_sha256 = $executableSha256
            cache_megabytes_requested = 512
            app_report = $appReport
        }
        Write-AtomicUtf8File -Path $finalReportPath -Contents ($report | ConvertTo-Json -Depth 12)
    } catch {
        Write-Warning "Could not publish failed playback-disruptions report: $($_.Exception.Message)"
    }
}

try {
    if (-not [IO.Path]::IsPathRooted($LauncherPath)) { throw "LauncherPath must be the exact full path $approvedLauncherPath." }
    $resolvedLauncher = [IO.Path]::GetFullPath($LauncherPath)
    if (-not [string]::Equals($resolvedLauncher, $approvedLauncherPath, [StringComparison]::OrdinalIgnoreCase)) { throw "The only permitted launcher is $approvedLauncherPath." }
    if (-not (Test-Path -LiteralPath $resolvedLauncher -PathType Leaf)) { throw "Required Maelstrom launcher does not exist: $resolvedLauncher" }
    $launcherSha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $resolvedLauncher).Hash
    $resolvedExecutable = [IO.Path]::GetFullPath((Join-Path $repoRoot 'dist\Maelstrom-Windows-x64\Maelstrom.exe'))
    if (-not (Test-Path -LiteralPath $resolvedExecutable -PathType Leaf)) { throw "Packaged executable does not exist: $resolvedExecutable" }
    $executableSha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $resolvedExecutable).Hash
    $stage = 'packaged_runtime'
    $packageDirectory = [IO.Path]::GetDirectoryName($resolvedExecutable)
    $ffmpeg = Join-Path $packageDirectory 'ffmpeg.exe'
    $ffprobe = Join-Path $packageDirectory 'ffprobe.exe'
    $runtimeNames = @('avcodec-62.dll', 'avdevice-62.dll', 'avfilter-11.dll', 'avformat-62.dll', 'avutil-60.dll', 'swresample-6.dll', 'swscale-9.dll', 'libgcc_s_seh-1.dll', 'libstdc++-6.dll', 'libvpl.dll', 'libwinpthread-1.dll', 'vcruntime140.dll')
    foreach ($required in @($ffmpeg, $ffprobe) + @($runtimeNames | ForEach-Object { Join-Path $packageDirectory $_ })) {
        if (-not (Test-Path -LiteralPath $required -PathType Leaf)) { throw "Packaged runtime is incomplete: $required" }
    }
    if ($ValidateOnly) {
        [pscustomobject]@{
            validation = 'passed'; launcher_path = $resolvedLauncher; launcher_sha256 = $launcherSha256
            executable_path = $resolvedExecutable; executable_sha256 = $executableSha256
            ffmpeg_path = $ffmpeg; ffprobe_path = $ffprobe; launch_performed = $false
        }
        return
    }

    New-Item -ItemType Directory -Path $artifactDirectory -Force | Out-Null
    foreach ($staleArtifact in @($mediaPath, "$mediaPath.tmp", $appReportPath, $finalReportPath, $appReportTemporaryPath, $finalReportTemporaryPath, $exportPath, $exportTemporaryPath)) {
        if (-not (Remove-FileWithRetries -Path $staleArtifact)) { throw "Playback disruption runner cannot remove its stale artifact: $staleArtifact" }
    }
    # Use only the packaged sibling tools/runtimes; this clip's 1920x1080 RGBA Full frames are
    # deliberately large enough for the probe's 512 MiB decoded-frame-cache eviction schedule.
    $env:PATH = "$packageDirectory;$env:SystemRoot\System32;$env:SystemRoot"
    $stage = 'fixture_generation_codec'
    & $ffmpeg -hide_banner -y -f lavfi -i 'testsrc2=size=1920x1080:rate=30' -f lavfi -i 'sine=frequency=1000:sample_rate=48000' -t 60 -c:v mpeg4 -q:v 5 -c:a aac -movflags +faststart $mediaPath *> $null
    if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath $mediaPath -PathType Leaf)) { throw 'Packaged ffmpeg.exe could not create the deterministic Full-quality 60-second A/V clip.' }

    $stage = 'editor_launch_report_wait'
    $env:MAELSTROM_SMOKE_EDITOR = '1'
    $env:MAELSTROM_MEDIA_ACCEPTANCE_PATH = $mediaPath
    $env:MAELSTROM_PLAYBACK_DISRUPTION_REPORT = $appReportPath
    $env:MAELSTROM_PLAYBACK_DISRUPTION_EXPORT_PATH = $exportPath
    $env:MAELSTROM_LAUNCHER_WAIT = '1'
    $launcherProcess = Start-Process -FilePath $env:ComSpec -ArgumentList @('/d', '/c', ('call "{0}" --cache-mb=512' -f $resolvedLauncher)) -WorkingDirectory $repoRoot -WindowStyle Normal -PassThru
    $deadline = [DateTime]::UtcNow.AddSeconds(180)
    $editorProcess = $null
    while ($null -eq $editorProcess) {
        $editorProcess = Find-OwnedPackagedEditorProcess -LauncherProcessId $launcherProcess.Id -PackagedExecutable $resolvedExecutable
        if ($null -ne $editorProcess) { break }
        $launcherProcess.Refresh()
        if ($launcherProcess.HasExited) { throw "Maelstrom launcher exited before its packaged editor child was observed (code $($launcherProcess.ExitCode))." }
        if ([DateTime]::UtcNow -ge $deadline) { throw 'Maelstrom launcher did not start the packaged editor within the bounded startup allowance.' }
        Start-Sleep -Milliseconds 100
    }
    while (-not (Test-Path -LiteralPath $appReportPath -PathType Leaf)) {
        if ([DateTime]::UtcNow -ge $deadline) { throw 'Playback disruption schedule did not write its report before the 180-second deadline.' }
        Start-Sleep -Milliseconds 250
        $editorProcess.Refresh()
        if ($editorProcess.HasExited) { throw "Packaged Maelstrom exited before disruption report publication (code $($editorProcess.ExitCode))." }
    }

    $stage = 'app_report_schema'
    $appReport = Get-Content -LiteralPath $appReportPath -Raw | ConvertFrom-Json
    foreach ($property in @('schema_version', 'elapsed_ms', 'action_count', 'scrub_requests', 'snapshot_restores', 'snapshot_restore_frames', 'snapshot_restore_audio_restarts', 'cache_eviction_before', 'cache_eviction_after', 'cache_eviction_growth')) { Assert-JsonUnsignedIntegerProperty $appReport $property 'Playback disruption report' }
    if ($appReport.schema_version -ne 1 -or $appReport.success -ne $true -or $null -ne $appReport.failed_step -or $null -ne $appReport.failure -or
        $appReport.elapsed_ms -lt 1 -or $appReport.scrub_requests -ne 8 -or $appReport.snapshot_restores -ne 8 -or $appReport.snapshot_restore_frames -ne 8 -or $appReport.snapshot_restore_audio_restarts -ne 8 -or
        $appReport.decoder_error_observed -ne $true -or $appReport.offline_marked -ne $true -or $appReport.recovery_frame_presented -ne $true -or
        $appReport.cache_eviction_after -lt $appReport.cache_eviction_before -or $appReport.cache_eviction_growth -lt 1 -or
        $appReport.export_started -ne $true -or $appReport.export_progress_observed -ne $true -or $appReport.export_cancelled -ne $true -or $appReport.export_terminal_cleanup -ne $true -or
        $appReport.selected_preview_quality -ne 'Full' -or $appReport.resolved_preview_quality -ne 'Full') { throw 'Playback disruption report omitted required successful scrub, restore, offline/recovery, cache, export, or Full-quality evidence.' }

    $stage = 'app_report_runtime_diagnostics'
    $diagnostics = $appReport.runtime_diagnostics_delta
    foreach ($property in @('monitor_requests', 'monitor_completed_frames', 'monitor_presented_frames', 'monitor_dropped_frames', 'monitor_hold_events', 'monitor_late_frames', 'monitor_errors', 'native_viewer_uploads', 'fallback_viewer_uploads', 'audio_underrun_frames', 'audio_callback_lock_failures', 'audio_late_discarded_frames')) { Assert-JsonUnsignedIntegerProperty $diagnostics $property 'Playback disruption runtime diagnostics' }
    if (-not (Test-JsonFiniteNumber $diagnostics.monitor_turnaround_window_p95_ms) -or $diagnostics.monitor_turnaround_window_p95_ms -lt 0 -or
        $diagnostics.monitor_requests -lt 1 -or $diagnostics.monitor_completed_frames -lt 1 -or $diagnostics.monitor_presented_frames -lt 1 -or $diagnostics.native_viewer_uploads -lt 1 -or
        $diagnostics.monitor_errors -ne 1 -or $diagnostics.fallback_viewer_uploads -ne 0 -or $diagnostics.audio_underrun_frames -ne 0 -or $diagnostics.audio_callback_lock_failures -ne 0 -or $diagnostics.audio_late_discarded_frames -ne 0 -or
        $diagnostics.monitor_late_frames -gt [Math]::Ceiling([double]$diagnostics.monitor_requests * 0.05)) { throw 'Playback disruption diagnostics exceeded bounds or did not record exactly the expected intentional offline-source monitor error.' }

    $stage = 'report_publication'
    $report = [ordered]@{
        schema_version = 1; status = 'passed'; failure = $null
        launcher_path = $resolvedLauncher; launcher_sha256 = $launcherSha256
        executable_path = $resolvedExecutable; executable_sha256 = $executableSha256
        cache_megabytes_requested = 512; app_report = $appReport
    }
    Write-AtomicUtf8File -Path $finalReportPath -Contents ($report | ConvertTo-Json -Depth 12)
    Write-Host "Playback disruption schedule passed: $($appReport.elapsed_ms) ms, $($appReport.cache_eviction_growth) cache evictions."
    Get-Item -LiteralPath $finalReportPath
} catch {
    if (-not $ValidateOnly) { Write-FailedPlaybackDisruptionReport -Failure $_ }
    throw
} finally {
    if ($launcherProcess) {
        try { & "$env:SystemRoot\System32\taskkill.exe" /PID $launcherProcess.Id /T /F *> $null } catch {}
        try { Wait-Process -Id $launcherProcess.Id -ErrorAction SilentlyContinue } catch {}
    }
    Restore-EnvironmentValue -Name 'PATH' -Value $savedPath
    foreach ($name in $savedValues.Keys) { Restore-EnvironmentValue -Name $name -Value $savedValues[$name] }
    if (-not $ValidateOnly) {
        foreach ($temporaryArtifact in @($mediaPath, "$mediaPath.tmp", $appReportTemporaryPath, $finalReportTemporaryPath, $exportTemporaryPath)) {
            if (-not (Remove-FileWithRetries -Path $temporaryArtifact)) { Write-Warning "Could not remove playback-disruptions temporary artifact: $temporaryArtifact" }
        }
    }
}
