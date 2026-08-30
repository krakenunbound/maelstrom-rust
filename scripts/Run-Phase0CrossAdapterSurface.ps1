#requires -Version 7.0
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$ExecutablePath,
    [string]$ReportPath,
    [ValidateRange(30, 300)]
    [int]$TimeoutSeconds = 90
)

$ErrorActionPreference = 'Stop'

function Restore-EnvironmentValue {
    param([Parameter(Mandatory = $true)][string]$Name, $Value)
    if ($null -eq $Value) {
        Remove-Item "Env:$Name" -ErrorAction SilentlyContinue
    } else {
        Set-Item "Env:$Name" $Value
    }
}

function Write-AtomicUtf8File {
    param([Parameter(Mandatory = $true)][string]$Path, [Parameter(Mandatory = $true)][string]$Contents)
    $temporary = "$Path.tmp"
    [IO.File]::WriteAllText($temporary, $Contents, [Text.UTF8Encoding]::new($false))
    Move-Item -LiteralPath $temporary -Destination $Path -Force
}

function Test-FiniteNonnegativeNumber($Value) {
    if ($null -eq $Value) { return $false }
    try { $number = [double]$Value } catch { return $false }
    -not [double]::IsNaN($number) -and -not [double]::IsInfinity($number) -and $number -ge 0
}

function Test-JsonUnsignedInteger($Value) {
    if ($null -eq $Value -or $Value -is [bool]) { return $false }
    try {
        $number = [double]$Value
        $integer = [uint64]$Value
    } catch {
        return $false
    }
    -not [double]::IsNaN($number) -and
        -not [double]::IsInfinity($number) -and
        $number -ge 0 -and
        $number -eq [double]$integer
}

function Assert-TimingStage {
    param(
        [Parameter(Mandatory = $true)]$Stage,
        [Parameter(Mandatory = $true)][string]$Context,
        [Parameter(Mandatory = $true)][string[]]$Properties
    )
    if ($null -eq $Stage) { throw "$Context is missing." }
    foreach ($property in $Properties) {
        if ($Stage.PSObject.Properties.Name -notcontains $property -or
            -not (Test-FiniteNonnegativeNumber $Stage.$property)) {
            throw "$Context has an invalid $property value."
        }
    }
    if ([int]$Stage.samples -lt 1) { throw "$Context was not exercised." }
}

function Stop-TrackedProcessTree {
    param($Process)
    if ($null -eq $Process) { return }
    try {
        & "$env:SystemRoot\System32\taskkill.exe" /PID $Process.Id /T /F *> $null
    } catch {
    }
    try { Wait-Process -Id $Process.Id -ErrorAction SilentlyContinue } catch {}
}

function Remove-GeneratedFile {
    param([Parameter(Mandatory = $true)][string]$Path)
    for ($attempt = 0; $attempt -lt 20; $attempt++) {
        if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { return }
        try {
            Remove-Item -LiteralPath $Path -Force -ErrorAction Stop
            return
        } catch {
            Start-Sleep -Milliseconds 100
        }
    }
    Write-Warning "Could not remove generated qualification file after process shutdown: $Path"
}

if (-not [IO.Path]::IsPathRooted($ExecutablePath)) {
    throw 'ExecutablePath must be the full absolute path to packaged Maelstrom.exe.'
}
$resolvedExecutable = [IO.Path]::GetFullPath($ExecutablePath)
if (-not (Test-Path -LiteralPath $resolvedExecutable -PathType Leaf) -or
    [IO.Path]::GetFileName($resolvedExecutable) -ine 'Maelstrom.exe') {
    throw "Packaged Maelstrom executable is missing or invalid: $resolvedExecutable"
}
$packageDirectory = [IO.Path]::GetDirectoryName($resolvedExecutable)
$ffmpeg = Join-Path $packageDirectory 'ffmpeg.exe'
$ffprobe = Join-Path $packageDirectory 'ffprobe.exe'
$requiredRuntimes = @(
    'avcodec-62.dll', 'avdevice-62.dll', 'avfilter-11.dll', 'avformat-62.dll',
    'avutil-60.dll', 'swresample-6.dll', 'swscale-9.dll', 'libgcc_s_seh-1.dll',
    'libstdc++-6.dll', 'libvpl.dll', 'libwinpthread-1.dll', 'vcruntime140.dll'
)
$requiredFiles = @($ffmpeg, $ffprobe) + @($requiredRuntimes | ForEach-Object { Join-Path $packageDirectory $_ })
foreach ($required in $requiredFiles) {
    if (-not (Test-Path -LiteralPath $required -PathType Leaf)) {
        throw "Packaged runtime is incomplete: $required"
    }
}

$repoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$artifactRoot = [IO.Path]::GetFullPath((Join-Path $repoRoot 'artifacts\phase0-cross-adapter-surface'))
if ([string]::IsNullOrWhiteSpace($ReportPath)) {
    $ReportPath = Join-Path $artifactRoot 'phase0-cross-adapter-surface.json'
}
$resolvedReportPath = if ([IO.Path]::IsPathRooted($ReportPath)) {
    [IO.Path]::GetFullPath($ReportPath)
} else {
    [IO.Path]::GetFullPath((Join-Path $repoRoot $ReportPath))
}
if (-not [string]::Equals([IO.Path]::GetDirectoryName($resolvedReportPath), $artifactRoot, [StringComparison]::OrdinalIgnoreCase) -or
    [IO.Path]::GetExtension($resolvedReportPath) -ine '.json') {
    throw "Report output must be a JSON file directly inside the ignored artifact directory: $artifactRoot"
}

$savedEnvironment = @{}
foreach ($name in @(
    'PATH', 'MAELSTROM_SMOKE_EDITOR', 'MAELSTROM_STARTUP_REPORT',
    'MAELSTROM_SURFACE_SUBMISSION_REPORT', 'MAELSTROM_MEDIA_ACCEPTANCE_PATH',
    'MAELSTROM_MEDIA_ACCEPTANCE_REPORT', 'MAELSTROM_MEDIA_ACCEPTANCE_EXPORT_PATH',
    'MAELSTROM_PHASE0_SURFACE_ADAPTER_CLASS'
)) {
    $savedEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
}

$runLock = [Threading.Mutex]::new($false, 'Local\MaelstromRustPhase0CrossAdapterSurfaceLock')
$lockAcquired = $false
$mediaPath = Join-Path $artifactRoot 'deterministic-av-60s.mp4'
$runs = [Collections.Generic.List[object]]::new()
try {
    if (-not $runLock.WaitOne(0)) { throw 'Another Phase 0 cross-adapter surface run is active.' }
    $lockAcquired = $true
    New-Item -ItemType Directory -Path $artifactRoot -Force | Out-Null
    Remove-Item -LiteralPath $resolvedReportPath -Force -ErrorAction SilentlyContinue
    $env:PATH = "$packageDirectory;$env:SystemRoot\System32;$env:SystemRoot"
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
        throw 'Packaged ffmpeg.exe could not create the deterministic A/V qualification clip.'
    }

    foreach ($adapterClass in @('IntegratedGpu', 'DiscreteGpu')) {
        $prefix = $adapterClass.ToLowerInvariant()
        $startupPath = Join-Path $artifactRoot "$prefix-startup.json"
        $surfacePath = Join-Path $artifactRoot "$prefix-surface-schema7.json"
        $mediaReportPath = Join-Path $artifactRoot "$prefix-media-acceptance.json"
        $exportPath = Join-Path $artifactRoot "$prefix-cancelled-export.mp4"
        Remove-Item -LiteralPath $startupPath, $surfacePath, $mediaReportPath, $exportPath -Force -ErrorAction SilentlyContinue

        $env:MAELSTROM_SMOKE_EDITOR = '1'
        $env:MAELSTROM_STARTUP_REPORT = $startupPath
        $env:MAELSTROM_SURFACE_SUBMISSION_REPORT = $surfacePath
        $env:MAELSTROM_MEDIA_ACCEPTANCE_PATH = $mediaPath
        $env:MAELSTROM_MEDIA_ACCEPTANCE_REPORT = $mediaReportPath
        $env:MAELSTROM_MEDIA_ACCEPTANCE_EXPORT_PATH = $exportPath
        $env:MAELSTROM_PHASE0_SURFACE_ADAPTER_CLASS = $adapterClass
        $process = $null
        try {
            $process = Start-Process -FilePath $resolvedExecutable -WorkingDirectory $packageDirectory -WindowStyle Normal -PassThru
            $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
            while ((-not (Test-Path -LiteralPath $startupPath -PathType Leaf) -or
                -not (Test-Path -LiteralPath $surfacePath -PathType Leaf) -or
                -not (Test-Path -LiteralPath $mediaReportPath -PathType Leaf)) -and
                [DateTime]::UtcNow -lt $deadline) {
                Start-Sleep -Milliseconds 100
                $process.Refresh()
                if ($process.HasExited) {
                    throw "Packaged Maelstrom exited during the $adapterClass surface run with code $($process.ExitCode)."
                }
            }
            foreach ($requiredReport in @($startupPath, $surfacePath, $mediaReportPath)) {
                if (-not (Test-Path -LiteralPath $requiredReport -PathType Leaf)) {
                    throw "$adapterClass did not produce required report: $requiredReport"
                }
            }

            $startup = Get-Content -LiteralPath $startupPath -Raw | ConvertFrom-Json
            $surface = Get-Content -LiteralPath $surfacePath -Raw | ConvertFrom-Json
            $media = Get-Content -LiteralPath $mediaReportPath -Raw | ConvertFrom-Json
            if (-not (Test-FiniteNonnegativeNumber $startup.first_surface_present_ms) -or
                [double]$startup.first_surface_present_ms -ge 1000.0) {
                throw "$adapterClass first surface presentation regressed to $($startup.first_surface_present_ms) ms."
            }
            if ($surface.schema_version -ne 7 -or $surface.samples -lt 120 -or
                $surface.renderer_device_type -ne $adapterClass -or $surface.renderer_backend -ne 'Dx12' -or
                [string]::IsNullOrWhiteSpace([string]$surface.renderer_gpu_name) -or
                @($surface.decoder_backends).Count -lt 1 -or
                [string]::IsNullOrWhiteSpace([string]$surface.encoder_backend) -or
                $surface.encoder_backend -eq 'not_observed') {
                throw "$adapterClass surface report omitted required schema-7 renderer/media evidence."
            }
            if (-not (Test-FiniteNonnegativeNumber $surface.cpu_p95_ms) -or $surface.cpu_p95_ms -gt 8.0 -or
                -not (Test-FiniteNonnegativeNumber $surface.surface_submission_interval_p95_ms) -or
                $surface.surface_submission_interval_p95_ms -gt 25.0 -or
                -not (Test-FiniteNonnegativeNumber $surface.average_submission_fps) -or
                $surface.average_submission_fps -lt 55.0) {
                throw "$adapterClass surface cadence exceeded the retained foundation gate."
            }
            foreach ($stageName in @('cache_lookup', 'demux_packet', 'decoder_calls', 'scaler', 'rgba_copy_letterbox', 'worker_request')) {
                Assert-TimingStage -Stage $surface.decoder_stage_timings.$stageName -Context "$adapterClass decoder stage $stageName" -Properties @('samples', 'total_ms', 'mean_ms', 'max_ms')
            }
            foreach ($stageName in @('upload_cpu', 'compositor_encode_cpu')) {
                Assert-TimingStage -Stage $surface.viewer_stage_timings.$stageName -Context "$adapterClass viewer stage $stageName" -Properties @('samples', 'p95_ms', 'max_ms')
            }
            foreach ($stageName in @('output_callback_cpu', 'mix_render_cpu')) {
                Assert-TimingStage -Stage $surface.audio_stage_timings.$stageName -Context "$adapterClass audio stage $stageName" -Properties @('samples', 'total_ms', 'mean_ms', 'max_ms')
            }
            Assert-TimingStage -Stage $surface.gpu_stage_timings.submission_to_completion_elapsed -Context "$adapterClass GPU submission completion" -Properties @('samples', 'p95_ms', 'max_ms')
            if ($surface.gpu_stage_timings.timestamp_query_supported) {
                Assert-TimingStage -Stage $surface.gpu_stage_timings.composite_pass_gpu -Context "$adapterClass compositor GPU pass" -Properties @('samples', 'p95_ms', 'max_ms')
            } elseif ($null -ne $surface.gpu_stage_timings.composite_pass_gpu) {
                throw "$adapterClass serialized compositor GPU timing despite unavailable timestamp queries."
            }
            foreach ($property in @(
                'monitor_requests', 'monitor_completed_frames', 'monitor_presented_frames',
                'monitor_dropped_frames', 'monitor_hold_events', 'monitor_late_frames', 'monitor_errors',
                'native_viewer_uploads', 'fallback_viewer_uploads', 'audio_underrun_frames',
                'audio_callback_lock_failures', 'audio_late_discarded_frames'
            )) {
                if ($surface.runtime_diagnostics.PSObject.Properties.Name -notcontains $property -or
                    -not (Test-JsonUnsignedInteger $surface.runtime_diagnostics.$property)) {
                    throw "$adapterClass runtime diagnostics omitted or invalidated unsigned integer $property."
                }
            }
            foreach ($property in @('monitor_requests', 'monitor_completed_frames', 'monitor_presented_frames', 'native_viewer_uploads')) {
                if ($surface.runtime_diagnostics.$property -lt 1) {
                    throw "$adapterClass full media run did not exercise runtime diagnostic $property."
                }
            }
            if (($surface.runtime_diagnostics.native_viewer_uploads + $surface.runtime_diagnostics.fallback_viewer_uploads) -ne
                $surface.runtime_diagnostics.monitor_presented_frames) {
                throw "$adapterClass reported inconsistent viewer uploads and presented frames."
            }
            foreach ($property in @('monitor_errors', 'audio_callback_lock_failures')) {
                if ($surface.runtime_diagnostics.$property -ne 0) {
                    throw "$adapterClass reported a disqualifying runtime fault in $property."
                }
            }
            foreach ($property in @('media_pool_drag_completed', 'analysis_metadata_ready', 'waveform_ready', 'monitor_frame_arrived', 'native_viewer_uploaded', 'live_audio_meter_nonzero', 'export_started', 'export_progress_received', 'export_cancelled')) {
                if ($media.$property -ne $true) { throw "$adapterClass media acceptance did not prove $property." }
            }
            if (Test-Path -LiteralPath $exportPath -PathType Leaf) {
                throw "$adapterClass cancelled export left a partial file."
            }
            $runs.Add([ordered]@{
                requested_device_type = $adapterClass
                renderer_gpu_name = [string]$surface.renderer_gpu_name
                renderer_vendor_id = [uint32]$surface.renderer_vendor_id
                renderer_device_id = [uint32]$surface.renderer_device_id
                renderer_backend = [string]$surface.renderer_backend
                renderer_driver = [string]$surface.renderer_driver
                startup_report = [IO.Path]::GetFullPath($startupPath)
                startup_report_sha256 = (Get-FileHash -LiteralPath $startupPath -Algorithm SHA256).Hash.ToLowerInvariant()
                surface_report = [IO.Path]::GetFullPath($surfacePath)
                surface_report_sha256 = (Get-FileHash -LiteralPath $surfacePath -Algorithm SHA256).Hash.ToLowerInvariant()
                media_acceptance_report = [IO.Path]::GetFullPath($mediaReportPath)
                media_acceptance_report_sha256 = (Get-FileHash -LiteralPath $mediaReportPath -Algorithm SHA256).Hash.ToLowerInvariant()
            })
            Write-Host "Phase 0 $adapterClass full surface report: PASS ($surfacePath)"
        } finally {
            Stop-TrackedProcessTree $process
            Remove-GeneratedFile -Path $exportPath
        }
    }

    $combined = [ordered]@{
        schema_version = 1
        status = 'passed'
        scope = 'packaged_editor_full_surface_schema7'
        executable_path = $resolvedExecutable
        executable_sha256 = (Get-FileHash -LiteralPath $resolvedExecutable -Algorithm SHA256).Hash.ToLowerInvariant()
        physical_scanout_observed = $false
        runs = $runs
    }
    Write-AtomicUtf8File -Path $resolvedReportPath -Contents (($combined | ConvertTo-Json -Depth 8) + "`n")
    Write-Host "Phase 0 cross-adapter full surface qualification: PASS ($resolvedReportPath)"
} finally {
    Remove-GeneratedFile -Path $mediaPath
    foreach ($entry in $savedEnvironment.GetEnumerator()) {
        Restore-EnvironmentValue -Name $entry.Key -Value $entry.Value
    }
    if ($lockAcquired) { $runLock.ReleaseMutex() }
    $runLock.Dispose()
}
