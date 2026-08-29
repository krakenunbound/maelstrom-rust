[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$FfmpegBundleRoot,
    [string]$LibClangPath = $env:LIBCLANG_PATH
)

$ErrorActionPreference = 'Stop'
$savedProcessPath = $env:PATH
$savedFfmpegDir = $env:FFMPEG_DIR
$savedLibClangPath = $env:LIBCLANG_PATH
try {
$repoRoot = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$output = [System.IO.Path]::GetFullPath((Join-Path $repoRoot 'dist\Maelstrom-Windows-x64'))
$distRoot = [System.IO.Path]::GetFullPath((Join-Path $repoRoot 'dist'))
$distPrefix = $distRoot.TrimEnd([System.IO.Path]::DirectorySeparatorChar) + [System.IO.Path]::DirectorySeparatorChar
if (-not $output.StartsWith($distPrefix, [System.StringComparison]::OrdinalIgnoreCase)) {
    throw "Refusing to package outside $distRoot"
}
$bundleRoot = [System.IO.Path]::GetFullPath($FfmpegBundleRoot)
$bundleBin = Join-Path $bundleRoot 'bin'
$ffmpeg = Join-Path $bundleBin 'ffmpeg.exe'
$ffprobe = Join-Path $bundleBin 'ffprobe.exe'
$buildManifest = Join-Path $bundleRoot 'BUILD-MANIFEST.txt'
$buildChecksums = Join-Path $bundleRoot 'BUILD-SHA256SUMS.txt'
if (-not (Test-Path -LiteralPath $ffmpeg) -or -not (Test-Path -LiteralPath $ffprobe)) {
    throw 'The FFmpeg bundle must contain bin\ffmpeg.exe and bin\ffprobe.exe.'
}
if (-not (Test-Path -LiteralPath $buildManifest) -or -not (Test-Path -LiteralPath $buildChecksums)) {
    throw 'Release packaging requires the project-built FFmpeg manifest and checksum inventory.'
}
$manifestText = Get-Content -LiteralPath $buildManifest -Raw
if ($manifestText -notmatch 'FFmpeg commit: 9047fa1b084f76b1b4d065af2d743df1b40dfb56' -or
    $manifestText -notmatch 'nv-codec-headers commit: 1889e62e2d35ff7aa9baca2bceb14f053785e6f1' -or
    $manifestText -notmatch 'oneVPL commit: 2274efcd3672b43297ef774f332e1fed6781381c') {
    throw 'The FFmpeg build manifest does not match Maelstrom''s pinned source revisions.'
}
foreach ($line in Get-Content -LiteralPath $buildChecksums) {
    if ($line -notmatch '^([0-9a-f]{64})  (.+)$') {
        throw "Malformed FFmpeg checksum entry: $line"
    }
    $expectedHash = $Matches[1].ToUpperInvariant()
    $relativePath = $Matches[2].Replace('/', [System.IO.Path]::DirectorySeparatorChar)
    $artifact = [System.IO.Path]::GetFullPath((Join-Path $bundleRoot $relativePath))
    if (-not $artifact.StartsWith($bundleRoot.TrimEnd([System.IO.Path]::DirectorySeparatorChar) + [System.IO.Path]::DirectorySeparatorChar, [System.StringComparison]::OrdinalIgnoreCase) -or
        -not (Test-Path -LiteralPath $artifact)) {
        throw "FFmpeg checksum target is missing or unsafe: $relativePath"
    }
    $actualHash = (Get-FileHash -LiteralPath $artifact -Algorithm SHA256).Hash
    if ($actualHash -ne $expectedHash) {
        throw "FFmpeg checksum mismatch: $relativePath"
    }
}

$configuration = (& $ffmpeg -hide_banner -version 2>&1 | Out-String)
if ($configuration -notmatch 'ffmpeg version n?8\.1' -or $configuration -notmatch '--enable-shared') {
    throw 'Packaging requires the frozen FFmpeg 8.1 shared-library line.'
}
if ($configuration -match '--enable-gpl' -or $configuration -match '--enable-nonfree') {
    throw 'Refusing to package GPL/nonfree FFmpeg. Supply the pinned LGPL shared bundle.'
}
$forbidden = @('libx264*.dll', 'libx265*.dll', '*fdk*aac*.dll')
foreach ($pattern in $forbidden) {
    if (Get-ChildItem -LiteralPath $bundleBin -Filter $pattern -ErrorAction SilentlyContinue) {
        throw "Refusing forbidden codec dependency: $pattern"
    }
}

$libClangCandidates = @(
    $LibClangPath,
    'C:\CraftRoot\bin',
    'C:\Program Files\LLVM\bin'
) | Where-Object { $_ }
$resolvedLibClang = $libClangCandidates | Where-Object {
    (Test-Path -LiteralPath (Join-Path $_ 'libclang.dll')) -or
    (Test-Path -LiteralPath (Join-Path $_ 'clang.dll'))
} | Select-Object -First 1
if (-not $resolvedLibClang) {
    throw 'libclang.dll is required to generate FFmpeg bindings. Pass -LibClangPath; it is a build tool and is not packaged.'
}

$env:FFMPEG_DIR = $bundleRoot
$env:LIBCLANG_PATH = [System.IO.Path]::GetFullPath($resolvedLibClang)
$env:PATH = "$bundleBin;$resolvedLibClang;$env:PATH"
Push-Location $repoRoot
try {
    cargo build -p nle-app --release
    if ($LASTEXITCODE -ne 0) { throw 'Release build failed.' }
} finally {
    Pop-Location
}

if (Test-Path -LiteralPath $output) {
    Remove-Item -LiteralPath $output -Recurse -Force
}
New-Item -ItemType Directory -Path $output -Force | Out-Null
Copy-Item -LiteralPath (Join-Path $repoRoot 'target\release\nle-app.exe') -Destination (Join-Path $output 'Maelstrom.exe')
Copy-Item -LiteralPath $ffmpeg -Destination $output
Copy-Item -LiteralPath $ffprobe -Destination $output
Get-ChildItem -LiteralPath $bundleBin -Filter '*.dll' | Copy-Item -Destination $output
Copy-Item -LiteralPath (Join-Path $repoRoot 'THIRD_PARTY_NOTICES.md') -Destination $output
Copy-Item -LiteralPath (Join-Path $bundleRoot 'LICENSE.txt') -Destination (Join-Path $output 'FFmpeg-LICENSE.txt')
Copy-Item -LiteralPath (Join-Path $bundleRoot 'oneVPL-LICENSE.txt') -Destination $output
Copy-Item -LiteralPath $buildManifest -Destination $output
Copy-Item -LiteralPath $buildChecksums -Destination $output
$modelSource = Join-Path $repoRoot 'assets\models'
$modelOutput = Join-Path $output 'models'
$modelManifestPath = Join-Path $modelSource 'manifest.json'
if (-not (Test-Path -LiteralPath $modelManifestPath)) {
    throw 'The packaged model registry requires assets\models\manifest.json.'
}
$modelManifest = Get-Content -LiteralPath $modelManifestPath -Raw | ConvertFrom-Json
if ($modelManifest.version -ne 1 -or $null -eq $modelManifest.models) {
    throw 'The packaged model registry must use manifest version 1 and contain a models array.'
}
if ($modelManifest.models.Count -gt 64) {
    throw "The packaged model registry contains $($modelManifest.models.Count) entries; maximum is 64."
}
$modelSourcePrefix = [System.IO.Path]::GetFullPath($modelSource).TrimEnd([System.IO.Path]::DirectorySeparatorChar) + [System.IO.Path]::DirectorySeparatorChar
$modelIds = @{}
foreach ($model in $modelManifest.models) {
    if ([string]::IsNullOrWhiteSpace($model.id) -or $modelIds.ContainsKey($model.id)) {
        throw "The packaged model registry contains an empty or duplicate id: $($model.id)"
    }
    $modelIds[$model.id] = $true
    if ([string]::IsNullOrWhiteSpace($model.file) -or [System.IO.Path]::IsPathRooted($model.file)) {
        throw "Model '$($model.id)' has an unsafe path: $($model.file)"
    }
    $modelPath = [System.IO.Path]::GetFullPath((Join-Path $modelSource $model.file))
    if (-not $modelPath.StartsWith($modelSourcePrefix, [System.StringComparison]::OrdinalIgnoreCase) -or
        -not (Test-Path -LiteralPath $modelPath -PathType Leaf)) {
        throw "Model '$($model.id)' is missing or outside assets\models: $($model.file)"
    }
    if ($null -ne $model.expected_bytes -and (Get-Item -LiteralPath $modelPath).Length -ne $model.expected_bytes) {
        throw "Model '$($model.id)' size does not match expected_bytes."
    }
}
Copy-Item -LiteralPath $modelSource -Destination $modelOutput -Recurse
if (-not (Test-Path -LiteralPath (Join-Path $modelOutput 'manifest.json'))) {
    throw 'The model manifest was not copied into the Windows package.'
}

$savedSmokePath = $env:PATH
$savedSmokeEditor = $env:MAELSTROM_SMOKE_EDITOR
$savedStartupReport = $env:MAELSTROM_STARTUP_REPORT
$savedSurfaceSubmissionReport = $env:MAELSTROM_SURFACE_SUBMISSION_REPORT
$savedMediaAcceptancePath = $env:MAELSTROM_MEDIA_ACCEPTANCE_PATH
$savedMediaAcceptanceReport = $env:MAELSTROM_MEDIA_ACCEPTANCE_REPORT
$savedMediaAcceptanceExportPath = $env:MAELSTROM_MEDIA_ACCEPTANCE_EXPORT_PATH
$startupReportPath = Join-Path $distRoot 'last-startup-smoke.json'
$surfaceSubmissionReportPath = Join-Path $distRoot 'last-surface-submission-smoke.json'
$mediaAcceptanceReportPath = Join-Path $distRoot 'last-media-acceptance-smoke.json'
$smokeMediaPath = Join-Path $distRoot 'packaged-media-acceptance-smoke.mp4'
$smokeExportPath = Join-Path $distRoot 'packaged-media-acceptance-export.mp4'
Remove-Item -LiteralPath $startupReportPath -Force -ErrorAction SilentlyContinue
Remove-Item -LiteralPath $surfaceSubmissionReportPath -Force -ErrorAction SilentlyContinue
Remove-Item -LiteralPath $mediaAcceptanceReportPath -Force -ErrorAction SilentlyContinue
Remove-Item -LiteralPath $smokeMediaPath -Force -ErrorAction SilentlyContinue
Remove-Item -LiteralPath $smokeExportPath -Force -ErrorAction SilentlyContinue
$smokeProcess = $null
try {
    $env:PATH = 'C:\Windows\System32;C:\Windows'
    $env:MAELSTROM_SMOKE_EDITOR = '1'
    $env:MAELSTROM_STARTUP_REPORT = $startupReportPath
    $env:MAELSTROM_SURFACE_SUBMISSION_REPORT = $surfaceSubmissionReportPath
    $env:MAELSTROM_MEDIA_ACCEPTANCE_PATH = $smokeMediaPath
    $env:MAELSTROM_MEDIA_ACCEPTANCE_REPORT = $mediaAcceptanceReportPath
    $env:MAELSTROM_MEDIA_ACCEPTANCE_EXPORT_PATH = $smokeExportPath
    & (Join-Path $output 'ffmpeg.exe') -hide_banner -version *> $null
    if ($LASTEXITCODE -ne 0) { throw 'Packaged ffmpeg.exe could not load using bundled DLLs.' }
    & (Join-Path $output 'ffprobe.exe') -hide_banner -version *> $null
    if ($LASTEXITCODE -ne 0) { throw 'Packaged ffprobe.exe could not load using bundled DLLs.' }
    # Windows PowerShell surfaces native stderr as ErrorRecord objects. FFmpeg writes ordinary
    # progress there, so temporarily keep those records non-terminating and judge the process by
    # its exit code plus the expected artifact instead.
    $savedErrorActionPreference = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    & (Join-Path $output 'ffmpeg.exe') -hide_banner -y `
        -f lavfi -i 'testsrc2=size=320x180:rate=30' `
        -f lavfi -i 'sine=frequency=1000:sample_rate=48000' `
        -t 60 -c:v mpeg4 -q:v 8 -c:a aac -movflags +faststart $smokeMediaPath *> $null
    $smokeEncodeExitCode = $LASTEXITCODE
    $ErrorActionPreference = $savedErrorActionPreference
    if ($smokeEncodeExitCode -ne 0 -or -not (Test-Path -LiteralPath $smokeMediaPath)) {
        throw 'Packaged ffmpeg.exe could not create the deterministic A/V acceptance clip.'
    }
    # A fresh release link and the acceptance-clip encode can leave short-lived compiler/linker
    # memory pressure behind even though both child processes have exited. Give Windows one quiet
    # scheduling interval before measuring presentation cadence; thresholds and sample count stay
    # unchanged, so sustained renderer regressions still fail the gate.
    Start-Sleep -Milliseconds 2000
    $smokeProcess = Start-Process -FilePath (Join-Path $output 'Maelstrom.exe') `
        -WorkingDirectory $output -WindowStyle Normal -PassThru
    $smokeDeadline = [DateTime]::UtcNow.AddSeconds(60)
    while ((-not (Test-Path -LiteralPath $startupReportPath) -or
        -not (Test-Path -LiteralPath $surfaceSubmissionReportPath) -or
        -not (Test-Path -LiteralPath $mediaAcceptanceReportPath)) -and
        [DateTime]::UtcNow -lt $smokeDeadline) {
        Start-Sleep -Milliseconds 100
        $smokeProcess.Refresh()
        if ($smokeProcess.HasExited) {
            throw "Packaged Maelstrom.exe exited during startup with code $($smokeProcess.ExitCode)."
        }
    }
    if (-not (Test-Path -LiteralPath $startupReportPath)) {
        throw 'Packaged editor did not report its first successful surface presentation.'
    }
    if (-not (Test-Path -LiteralPath $surfaceSubmissionReportPath)) {
        throw 'Packaged editor did not complete the 120-frame surface submission cadence probe.'
    }
    if (-not (Test-Path -LiteralPath $mediaAcceptanceReportPath)) {
        throw 'Packaged editor did not complete the real-media acceptance probe.'
    }
    $startup = Get-Content -LiteralPath $startupReportPath -Raw | ConvertFrom-Json
    if ($null -eq $startup.first_surface_present_ms -or
        $startup.first_surface_present_ms -lt 0 -or
        $startup.first_surface_present_ms -ge 1000.0) {
        throw "Packaged editor first surface presentation regressed to $($startup.first_surface_present_ms) ms."
    }
    $surfaceSubmission = Get-Content -LiteralPath $surfaceSubmissionReportPath -Raw | ConvertFrom-Json
    foreach ($property in @(
        'schema_version', 'samples', 'cpu_p95_ms', 'surface_submission_interval_p95_ms',
        'average_submission_fps', 'renderer_gpu_name', 'renderer_vendor_id', 'renderer_device_id',
        'renderer_backend', 'renderer_driver', 'renderer_driver_info', 'decoder_backends',
        'encoder_backend', 'cpu_identity', 'logical_cpu_count', 'total_physical_memory_bytes',
        'selected_preview_quality', 'resolved_preview_quality', 'preview_width', 'preview_height',
        'monitor_cache_cap_bytes', 'display_refresh_millihertz', 'decoder_stage_timings'
    )) {
        if ($surfaceSubmission.PSObject.Properties.Name -notcontains $property) {
            throw "Surface submission probe omitted $property."
        }
    }
    if ($surfaceSubmission.samples -lt 120) {
        throw "Surface submission probe returned only $($surfaceSubmission.samples) samples."
    }
    if ($surfaceSubmission.schema_version -ne 1) {
        throw "Surface submission probe returned unsupported schema $($surfaceSubmission.schema_version)."
    }
    if ($surfaceSubmission.cpu_p95_ms -lt 0 -or $surfaceSubmission.cpu_p95_ms -gt 8.0) {
        throw "Packaged editor CPU p95 regressed to $($surfaceSubmission.cpu_p95_ms) ms."
    }
    if ($surfaceSubmission.average_submission_fps -lt 55.0 -or $surfaceSubmission.surface_submission_interval_p95_ms -lt 0 -or $surfaceSubmission.surface_submission_interval_p95_ms -gt 25.0) {
        throw "Packaged editor surface submission cadence regressed: $($surfaceSubmission.average_submission_fps) submissions/s, p95 $($surfaceSubmission.surface_submission_interval_p95_ms) ms."
    }
    if ([string]::IsNullOrWhiteSpace($surfaceSubmission.renderer_gpu_name) -or
        [string]::IsNullOrWhiteSpace($surfaceSubmission.renderer_backend) -or
        [string]::IsNullOrWhiteSpace($surfaceSubmission.cpu_identity) -or
        $surfaceSubmission.logical_cpu_count -lt 1 -or
        $surfaceSubmission.total_physical_memory_bytes -lt 1 -or
        $surfaceSubmission.decoder_backends.Count -lt 1 -or
        [string]::IsNullOrWhiteSpace($surfaceSubmission.encoder_backend) -or
        $surfaceSubmission.encoder_backend -eq 'not_observed' -or
        $surfaceSubmission.preview_width -lt 1 -or $surfaceSubmission.preview_height -lt 1 -or
        $surfaceSubmission.monitor_cache_cap_bytes -lt 1 -or
        ($null -ne $surfaceSubmission.display_refresh_millihertz -and
            $surfaceSubmission.display_refresh_millihertz -lt 1)) {
        throw 'Surface submission probe returned incomplete performance environment metadata.'
    }
    foreach ($stageName in @('cache_lookup', 'demux_packet', 'decoder_calls', 'hardware_transfer', 'scaler', 'rgba_copy_letterbox', 'worker_request')) {
        $stage = $surfaceSubmission.decoder_stage_timings.$stageName
        if ($null -eq $stage) { throw "Surface submission probe omitted decoder stage $stageName." }
        foreach ($property in @('samples', 'total_ms', 'mean_ms', 'max_ms')) {
            if ($stage.PSObject.Properties.Name -notcontains $property) {
                throw "Decoder timing stage $stageName omitted $property."
            }
            $value = [double]$stage.$property
            if ([double]::IsNaN($value) -or [double]::IsInfinity($value) -or $value -lt 0) {
                throw "Decoder timing stage $stageName returned invalid ${property}: $value."
            }
        }
        if ($stage.max_ms -lt $stage.mean_ms) {
            throw "Decoder timing stage $stageName has max below mean."
        }
        if ($stage.samples -eq 0 -and ($stage.total_ms -ne 0 -or $stage.mean_ms -ne 0 -or $stage.max_ms -ne 0)) {
            throw "Decoder timing stage $stageName reported durations without samples."
        }
        if ($stage.samples -gt 0) {
            if ($stage.total_ms -lt $stage.max_ms) {
                throw "Decoder timing stage $stageName has total below max."
            }
            $expectedMean = [double]$stage.total_ms / [double]$stage.samples
            $meanTolerance = [Math]::Max(0.000001, [Math]::Abs($expectedMean) * 0.000000001)
            if ([Math]::Abs([double]$stage.mean_ms - $expectedMean) -gt $meanTolerance) {
                throw "Decoder timing stage $stageName has an inconsistent mean."
            }
        }
    }
    foreach ($stageName in @('cache_lookup', 'demux_packet', 'decoder_calls', 'scaler', 'rgba_copy_letterbox', 'worker_request')) {
        if ($surfaceSubmission.decoder_stage_timings.$stageName.samples -lt 1) {
            throw "Full media smoke did not exercise decoder timing stage $stageName."
        }
    }
    $mediaAcceptance = Get-Content -LiteralPath $mediaAcceptanceReportPath -Raw | ConvertFrom-Json
    foreach ($property in @('media_pool_drag_completed', 'analysis_metadata_ready', 'waveform_ready', 'monitor_frame_arrived', 'native_viewer_uploaded', 'live_audio_meter_nonzero', 'live_fade_reduced', 'live_fade_recovered', 'live_gain_reduced', 'export_started', 'export_progress_received', 'playhead_advanced_while_exporting', 'export_cancelled')) {
        if ($mediaAcceptance.$property -ne $true) { throw "Media acceptance probe did not prove $property." }
    }
    foreach ($property in @('viewer_panel_height', 'timeline_panel_height', 'timeline_view_span_ticks', 'timeline_end_ticks', 'linked_video_bars', 'linked_audio_bars', 'waveform_peak_count', 'playhead_advanced_ticks')) {
        if ($null -eq $mediaAcceptance.$property -or $mediaAcceptance.$property -le 0) {
            throw "Media acceptance probe returned invalid $($property): $($mediaAcceptance.$property)."
        }
    }
    if ($mediaAcceptance.timeline_panel_height -le (0.5 * $mediaAcceptance.viewer_panel_height) -or
        $mediaAcceptance.timeline_panel_height -ge (1.5 * $mediaAcceptance.viewer_panel_height)) {
        throw "Packaged default layout is unbalanced: viewer $($mediaAcceptance.viewer_panel_height) px, timeline $($mediaAcceptance.timeline_panel_height) px."
    }
    if ($mediaAcceptance.timeline_end_ticks -lt 59000000 -or $mediaAcceptance.timeline_end_ticks -gt 61000000) {
        throw "Packaged 60-second media duration was not reconciled: $($mediaAcceptance.timeline_end_ticks) ticks."
    }
    if ($mediaAcceptance.timeline_view_span_ticks -lt $mediaAcceptance.timeline_end_ticks -or
        $mediaAcceptance.timeline_view_span_ticks -gt (2 * $mediaAcceptance.timeline_end_ticks)) {
        throw "Packaged first placement is not fitted: view $($mediaAcceptance.timeline_view_span_ticks) ticks, content $($mediaAcceptance.timeline_end_ticks) ticks."
    }
    if ($mediaAcceptance.playhead_advanced_ticks -lt 500000) {
        throw "Media acceptance playhead advanced only $($mediaAcceptance.playhead_advanced_ticks) ticks."
    }
    if (Test-Path -LiteralPath $smokeExportPath) {
        throw 'Cancelled packaged export left a partial output behind.'
    }
    Write-Host "Startup smoke: first successful surface presentation in $($startup.first_surface_present_ms) ms"
    Write-Host "Surface submission smoke: $($surfaceSubmission.average_submission_fps) submissions/s, interval p95 $($surfaceSubmission.surface_submission_interval_p95_ms) ms, CPU p95 $($surfaceSubmission.cpu_p95_ms) ms (not scanout/GPU completion)"
    Write-Host "Media acceptance smoke: native viewer upload; balanced viewer/timeline $($mediaAcceptance.viewer_panel_height)/$($mediaAcceptance.timeline_panel_height) px; fitted view $($mediaAcceptance.timeline_view_span_ticks)/$($mediaAcceptance.timeline_end_ticks) ticks; $($mediaAcceptance.linked_video_bars) V bars, $($mediaAcceptance.linked_audio_bars) A bars, $($mediaAcceptance.waveform_peak_count) waveform peaks, $($mediaAcceptance.playhead_advanced_ticks) ticks, export cancelled cleanly"
} finally {
    if ($smokeProcess) {
        try {
            # Terminate the exact package-smoke process tree. Force-killing only the GUI parent
            # can bypass ExportJob::Drop and leave its FFmpeg child consuming system resources.
            & "$env:SystemRoot\System32\taskkill.exe" /PID $smokeProcess.Id /T /F *> $null
        } catch {
            # The app may have exited between the last refresh and cleanup.
        }
        try {
            Wait-Process -Id $smokeProcess.Id -ErrorAction SilentlyContinue
        } catch {
        }
    }
    $env:PATH = $savedSmokePath
    if ($null -eq $savedSmokeEditor) {
        Remove-Item Env:MAELSTROM_SMOKE_EDITOR -ErrorAction SilentlyContinue
    } else {
        $env:MAELSTROM_SMOKE_EDITOR = $savedSmokeEditor
    }
    if ($null -eq $savedStartupReport) {
        Remove-Item Env:MAELSTROM_STARTUP_REPORT -ErrorAction SilentlyContinue
    } else {
        $env:MAELSTROM_STARTUP_REPORT = $savedStartupReport
    }
    if ($null -eq $savedSurfaceSubmissionReport) {
        Remove-Item Env:MAELSTROM_SURFACE_SUBMISSION_REPORT -ErrorAction SilentlyContinue
    } else {
        $env:MAELSTROM_SURFACE_SUBMISSION_REPORT = $savedSurfaceSubmissionReport
    }
    if ($null -eq $savedMediaAcceptancePath) {
        Remove-Item Env:MAELSTROM_MEDIA_ACCEPTANCE_PATH -ErrorAction SilentlyContinue
    } else {
        $env:MAELSTROM_MEDIA_ACCEPTANCE_PATH = $savedMediaAcceptancePath
    }
    if ($null -eq $savedMediaAcceptanceReport) {
        Remove-Item Env:MAELSTROM_MEDIA_ACCEPTANCE_REPORT -ErrorAction SilentlyContinue
    } else {
        $env:MAELSTROM_MEDIA_ACCEPTANCE_REPORT = $savedMediaAcceptanceReport
    }
    if ($null -eq $savedMediaAcceptanceExportPath) {
        Remove-Item Env:MAELSTROM_MEDIA_ACCEPTANCE_EXPORT_PATH -ErrorAction SilentlyContinue
    } else {
        $env:MAELSTROM_MEDIA_ACCEPTANCE_EXPORT_PATH = $savedMediaAcceptanceExportPath
    }
    Remove-Item -LiteralPath $smokeMediaPath -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $smokeExportPath -Force -ErrorAction SilentlyContinue
}

Get-Item -LiteralPath (Join-Path $output 'Maelstrom.exe')
} finally {
    $env:PATH = $savedProcessPath
    if ($null -eq $savedFfmpegDir) {
        Remove-Item Env:FFMPEG_DIR -ErrorAction SilentlyContinue
    } else {
        $env:FFMPEG_DIR = $savedFfmpegDir
    }
    if ($null -eq $savedLibClangPath) {
        Remove-Item Env:LIBCLANG_PATH -ErrorAction SilentlyContinue
    } else {
        $env:LIBCLANG_PATH = $savedLibClangPath
    }
}
