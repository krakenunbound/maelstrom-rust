[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$FfmpegBundleRoot,
    [string]$LibClangPath = $env:LIBCLANG_PATH,
    [string]$VcRedistCrtDirectory,
    [Parameter(HelpMessage = 'Build and assemble the package without running the GUI smoke checks. The resulting package is unqualified and existing dist\last-*-smoke.json reports are historical.')]
    [switch]$SkipSmoke
)

$ErrorActionPreference = 'Stop'

function Test-JsonIntegerValue {
    param($Value)
    return $Value -is [byte] -or $Value -is [sbyte] -or
        $Value -is [int16] -or $Value -is [uint16] -or
        $Value -is [int32] -or $Value -is [uint32] -or
        $Value -is [int64] -or $Value -is [uint64]
}

function Test-JsonFiniteNumber {
    param($Value)
    $numeric = (Test-JsonIntegerValue $Value) -or $Value -is [single] -or
        $Value -is [double] -or $Value -is [decimal]
    if (-not $numeric) { return $false }
    $doubleValue = [double]$Value
    return -not [double]::IsNaN($doubleValue) -and -not [double]::IsInfinity($doubleValue)
}

function Resolve-VcRedistCrtDirectory {
    param(
        [string]$ExplicitDirectory
    )

    function Test-VcRedistCrtDirectory {
        param(
            [string]$Directory,
            [string]$SourceDescription
        )
        $resolved = [System.IO.Path]::GetFullPath($Directory)
        $leaf = Split-Path -Leaf $resolved
        if ($leaf -notlike 'Microsoft.VC*.CRT') {
            throw "$SourceDescription must name a Microsoft.VC*.CRT directory: $resolved"
        }
        $runtime = Join-Path $resolved 'vcruntime140.dll'
        if (-not (Test-Path -LiteralPath $runtime -PathType Leaf)) {
            throw "$SourceDescription does not contain vcruntime140.dll: $resolved"
        }
        $peBytes = [System.IO.File]::ReadAllBytes($runtime)
        if ($peBytes.Length -lt 64 -or
            [System.BitConverter]::ToUInt16($peBytes, 0) -ne 0x5A4D) {
            throw "$SourceDescription contains an invalid PE runtime: $runtime"
        }
        $peHeaderOffset = [System.BitConverter]::ToInt32($peBytes, 0x3C)
        if ($peHeaderOffset -lt 0 -or $peHeaderOffset + 6 -gt $peBytes.Length -or
            [System.BitConverter]::ToUInt32($peBytes, $peHeaderOffset) -ne 0x00004550 -or
            [System.BitConverter]::ToUInt16($peBytes, $peHeaderOffset + 4) -ne 0x8664) {
            throw "$SourceDescription must contain an AMD64 vcruntime140.dll: $runtime"
        }
        return $resolved
    }

    if (-not [string]::IsNullOrWhiteSpace($ExplicitDirectory)) {
        return Test-VcRedistCrtDirectory -Directory $ExplicitDirectory -SourceDescription '-VcRedistCrtDirectory'
    }

    $installationRoots = New-Object System.Collections.Generic.List[string]
    $programFilesX86 = ${env:ProgramFiles(x86)}
    $vswherePaths = @(
        (Join-Path $programFilesX86 'Microsoft Visual Studio\Installer\vswhere.exe'),
        'C:\Program Files (x86)\Microsoft Visual Studio\Installer\vswhere.exe'
    ) | Select-Object -Unique
    foreach ($vswhere in $vswherePaths) {
        if (-not (Test-Path -LiteralPath $vswhere -PathType Leaf)) { continue }
        $reportedRoots = & $vswhere -products * -property installationPath -format value 2>$null
        foreach ($reportedRoot in $reportedRoots) {
            if (-not [string]::IsNullOrWhiteSpace($reportedRoot) -and (Test-Path -LiteralPath $reportedRoot -PathType Container)) {
                $installationRoots.Add([System.IO.Path]::GetFullPath($reportedRoot))
            }
        }
    }

    # Also support standard Visual Studio layouts when vswhere is not installed. These roots are
    # intentionally limited to Microsoft Visual Studio installation directories, never System32.
    foreach ($base in @('C:\Program Files\Microsoft Visual Studio', 'C:\Program Files (x86)\Microsoft Visual Studio')) {
        if (-not (Test-Path -LiteralPath $base -PathType Container)) { continue }
        Get-ChildItem -LiteralPath $base -Directory -ErrorAction SilentlyContinue | ForEach-Object {
            Get-ChildItem -LiteralPath $_.FullName -Directory -ErrorAction SilentlyContinue | ForEach-Object {
                $installationRoots.Add($_.FullName)
            }
        }
    }

    $candidates = New-Object System.Collections.Generic.List[object]
    foreach ($installationRoot in ($installationRoots | Select-Object -Unique)) {
        $redistRoot = Join-Path $installationRoot 'VC\Redist\MSVC'
        if (-not (Test-Path -LiteralPath $redistRoot -PathType Container)) { continue }
        foreach ($versionDirectory in (Get-ChildItem -LiteralPath $redistRoot -Directory -ErrorAction SilentlyContinue)) {
            $crtRoot = Join-Path $versionDirectory.FullName 'x64'
            if (-not (Test-Path -LiteralPath $crtRoot -PathType Container)) { continue }
            foreach ($crtDirectory in (Get-ChildItem -LiteralPath $crtRoot -Directory -Filter 'Microsoft.VC*.CRT' -ErrorAction SilentlyContinue)) {
                $runtime = Join-Path $crtDirectory.FullName 'vcruntime140.dll'
                if (-not (Test-Path -LiteralPath $runtime -PathType Leaf)) { continue }
                try {
                    $version = [version]$versionDirectory.Name
                } catch {
                    continue
                }
                $candidates.Add([pscustomobject]@{
                    Version = $version
                    Directory = [System.IO.Path]::GetFullPath($crtDirectory.FullName)
                })
            }
        }
    }

    $selected = $candidates | Sort-Object -Property @{ Expression = 'Version'; Descending = $true }, @{ Expression = 'Directory'; Descending = $false } | Select-Object -First 1
    if ($null -eq $selected) {
        throw 'vcruntime140.dll was not found in an installed Visual Studio x64 Microsoft.VC*.CRT Redist directory. Install the licensed Microsoft VC Redist/Visual Studio component or pass -VcRedistCrtDirectory with a trusted, authorized AMD64 CRT directory.'
    }
    return Test-VcRedistCrtDirectory -Directory $selected.Directory -SourceDescription 'Auto-discovered Visual Studio VC Redist directory'
}
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
$resolvedVcRedistCrt = Resolve-VcRedistCrtDirectory -ExplicitDirectory $VcRedistCrtDirectory
$vcRuntimeSource = Join-Path $resolvedVcRedistCrt 'vcruntime140.dll'
Write-Host "Using app-local Microsoft VC runtime from: $resolvedVcRedistCrt"

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
# vcruntime140.dll is intentionally excluded here: its only package source is the authorized
# Visual Studio CRT directory resolved above, never an FFmpeg bundle or Windows system folder.
Get-ChildItem -LiteralPath $bundleBin -Filter '*.dll' | Where-Object { $_.Name -ine 'vcruntime140.dll' } | Copy-Item -Destination $output
Copy-Item -LiteralPath $vcRuntimeSource -Destination (Join-Path $output 'vcruntime140.dll')
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

$requiredPackagedRuntimes = @(
    'avcodec-62.dll',
    'avdevice-62.dll',
    'avfilter-11.dll',
    'avformat-62.dll',
    'avutil-60.dll',
    'swresample-6.dll',
    'swscale-9.dll',
    'libgcc_s_seh-1.dll',
    'libstdc++-6.dll',
    'libvpl.dll',
    'libwinpthread-1.dll',
    'vcruntime140.dll'
)
foreach ($runtimeName in $requiredPackagedRuntimes) {
    if (-not (Test-Path -LiteralPath (Join-Path $output $runtimeName) -PathType Leaf)) {
        throw "Packaged Maelstrom runtime is incomplete: $runtimeName is missing beside Maelstrom.exe."
    }
}
$packageExePath = Join-Path $output 'Maelstrom.exe'
$packageStatusPath = Join-Path $output 'PACKAGE-STATUS.json'
$packageStatus = [ordered]@{
    schema_version = 1
    packaged_at_utc = [DateTime]::UtcNow.ToString('o')
    executable = 'Maelstrom.exe'
    executable_sha256 = (Get-FileHash -LiteralPath $packageExePath -Algorithm SHA256).Hash
    smoke_status = if ($SkipSmoke) { 'not_run' } else { 'not_passed' }
}
$packageStatus | ConvertTo-Json | Set-Content -LiteralPath $packageStatusPath -Encoding utf8
if ($SkipSmoke) {
    Write-Warning 'Skipped GUI smoke checks. This new package is unqualified; dist\last-*-smoke.json reports are historical.'
    return (Get-Item -LiteralPath $packageExePath)
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
        'surface_present_call_cpu_p95_ms',
        'average_submission_fps', 'renderer_gpu_name', 'renderer_vendor_id', 'renderer_device_id',
        'renderer_device_type',
        'renderer_backend', 'renderer_driver', 'renderer_driver_info', 'decoder_backends',
        'encoder_backend', 'cpu_identity', 'logical_cpu_count', 'total_physical_memory_bytes',
        'selected_preview_quality', 'resolved_preview_quality', 'preview_width', 'preview_height',
        'monitor_cache_cap_bytes', 'display_refresh_millihertz', 'observation_scope', 'decoder_stage_timings',
        'viewer_stage_timings', 'gpu_stage_timings', 'audio_stage_timings', 'runtime_diagnostics'
    )) {
        if ($surfaceSubmission.PSObject.Properties.Name -notcontains $property) {
            throw "Surface submission probe omitted $property."
        }
    }
    if ($surfaceSubmission.samples -lt 120) {
        throw "Surface submission probe returned only $($surfaceSubmission.samples) samples."
    }
    if ($surfaceSubmission.schema_version -ne 8) {
        throw "Surface submission probe returned unsupported schema $($surfaceSubmission.schema_version)."
    }
    $observationScope = $surfaceSubmission.observation_scope
    if ($null -eq $observationScope -or
        $observationScope.surface_submission_observed -ne $true -or
        $observationScope.surface_present_call_cpu_observed -ne $true -or
        $observationScope.gpu_submission_completion_observed -ne $true -or
        $observationScope.physical_scanout_observed -ne $false) {
        throw 'Surface submission probe returned an invalid or overstated observation scope.'
    }
    foreach ($property in @(
        'monitor_requests', 'monitor_completed_frames', 'monitor_presented_frames',
        'monitor_dropped_frames', 'monitor_hold_events', 'monitor_late_frames',
        'monitor_errors', 'native_viewer_uploads', 'fallback_viewer_uploads',
        'audio_underrun_frames', 'audio_callback_lock_failures', 'audio_late_discarded_frames'
    )) {
        if ($surfaceSubmission.runtime_diagnostics.PSObject.Properties.Name -notcontains $property -or
            -not (Test-JsonIntegerValue $surfaceSubmission.runtime_diagnostics.$property) -or
            $surfaceSubmission.runtime_diagnostics.$property -lt 0) {
            throw "Surface submission runtime diagnostics returned invalid unsigned integer ${property}."
        }
    }
    foreach ($property in @('monitor_requests', 'monitor_completed_frames', 'monitor_presented_frames', 'native_viewer_uploads')) {
        if ($surfaceSubmission.runtime_diagnostics.$property -lt 1) {
            throw "Full media smoke did not exercise runtime diagnostic $property."
        }
    }
    if (($surfaceSubmission.runtime_diagnostics.native_viewer_uploads + $surfaceSubmission.runtime_diagnostics.fallback_viewer_uploads) -ne
        $surfaceSubmission.runtime_diagnostics.monitor_presented_frames) {
        throw 'Surface submission runtime diagnostics reported inconsistent viewer uploads and presented frames.'
    }
    if ($surfaceSubmission.cpu_p95_ms -lt 0 -or $surfaceSubmission.cpu_p95_ms -gt 8.0) {
        throw "Packaged editor CPU p95 regressed to $($surfaceSubmission.cpu_p95_ms) ms."
    }
    if ([double]::IsNaN([double]$surfaceSubmission.surface_present_call_cpu_p95_ms) -or
        [double]::IsInfinity([double]$surfaceSubmission.surface_present_call_cpu_p95_ms) -or
        $surfaceSubmission.surface_present_call_cpu_p95_ms -lt 0) {
        throw "Packaged editor reported invalid surface present-call CPU p95 $($surfaceSubmission.surface_present_call_cpu_p95_ms) ms."
    }
    if ($surfaceSubmission.average_submission_fps -lt 55.0 -or $surfaceSubmission.surface_submission_interval_p95_ms -lt 0 -or $surfaceSubmission.surface_submission_interval_p95_ms -gt 25.0) {
        throw "Packaged editor surface submission cadence regressed: $($surfaceSubmission.average_submission_fps) submissions/s, p95 $($surfaceSubmission.surface_submission_interval_p95_ms) ms."
    }
    if ([string]::IsNullOrWhiteSpace($surfaceSubmission.renderer_gpu_name) -or
        [string]::IsNullOrWhiteSpace($surfaceSubmission.renderer_backend) -or
        $surfaceSubmission.renderer_device_type -notin @('IntegratedGpu', 'DiscreteGpu', 'VirtualGpu', 'Cpu', 'Other') -or
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
    foreach ($stageName in @('output_callback_cpu', 'mix_render_cpu')) {
        $stage = $surfaceSubmission.audio_stage_timings.$stageName
        if ($null -eq $stage) { throw "Surface submission probe omitted audio stage $stageName." }
        foreach ($property in @('samples', 'total_ms', 'mean_ms', 'max_ms')) {
            if ($stage.PSObject.Properties.Name -notcontains $property) {
                throw "Audio timing stage $stageName omitted $property."
            }
            if ($property -eq 'samples') {
                if (-not (Test-JsonIntegerValue $stage.$property) -or $stage.$property -lt 0) {
                    throw "Audio timing stage $stageName returned invalid unsigned integer ${property}: $($stage.$property)."
                }
            } elseif (-not (Test-JsonFiniteNumber $stage.$property)) {
                throw "Audio timing stage $stageName returned invalid numeric ${property}: $($stage.$property)."
            }
            $value = [double]$stage.$property
            if ($value -lt 0) {
                throw "Audio timing stage $stageName returned invalid ${property}: $value."
            }
        }
        if ($stage.max_ms -lt $stage.mean_ms) {
            throw "Audio timing stage $stageName has max below mean."
        }
        if ($stage.samples -eq 0 -and ($stage.total_ms -ne 0 -or $stage.mean_ms -ne 0 -or $stage.max_ms -ne 0)) {
            throw "Audio timing stage $stageName reported durations without samples."
        }
        if ($stage.samples -gt 0) {
            if ($stage.total_ms -lt $stage.max_ms) {
                throw "Audio timing stage $stageName has total below max."
            }
            $expectedMean = [double]$stage.total_ms / [double]$stage.samples
            $meanTolerance = [Math]::Max(0.000001, [Math]::Abs($expectedMean) * 0.000000001)
            if ([Math]::Abs([double]$stage.mean_ms - $expectedMean) -gt $meanTolerance) {
                throw "Audio timing stage $stageName has an inconsistent mean."
            }
        }
        if ($stage.samples -lt 1) {
            throw "Full media smoke did not exercise audio timing stage $stageName."
        }
    }
    foreach ($stageName in @('upload_cpu', 'compositor_encode_cpu')) {
        $stage = $surfaceSubmission.viewer_stage_timings.$stageName
        if ($null -eq $stage) { throw "Surface submission probe omitted viewer timing stage $stageName." }
        foreach ($property in @('samples', 'p95_ms', 'max_ms')) {
            if ($stage.PSObject.Properties.Name -notcontains $property) {
                throw "Viewer timing stage $stageName omitted $property."
            }
            $value = [double]$stage.$property
            if ([double]::IsNaN($value) -or [double]::IsInfinity($value) -or $value -lt 0) {
                throw "Viewer timing stage $stageName returned invalid ${property}: $value."
            }
        }
        if ($stage.max_ms -lt $stage.p95_ms) {
            throw "Viewer timing stage $stageName has max below p95."
        }
        if ($stage.samples -lt 1) {
            throw "Full media smoke did not exercise viewer timing stage $stageName."
        }
    }
    $gpuStages = $surfaceSubmission.gpu_stage_timings
    foreach ($property in @('timestamp_query_supported', 'composite_pass_gpu', 'submission_to_completion_elapsed')) {
        if ($gpuStages.PSObject.Properties.Name -notcontains $property) {
            throw "Surface submission probe omitted GPU timing field $property."
        }
    }
    if (-not ($gpuStages.timestamp_query_supported -is [bool])) {
        throw 'GPU timestamp-query support must be a boolean.'
    }
    $gpuComposite = $gpuStages.composite_pass_gpu
    if ($gpuStages.timestamp_query_supported) {
        if ($null -eq $gpuComposite) {
            throw 'Timestamp-query hardware omitted isolated compositor-pass timing.'
        }
        foreach ($property in @('samples', 'p95_ms', 'max_ms')) {
            if ($gpuComposite.PSObject.Properties.Name -notcontains $property) {
                throw "GPU compositor-pass timing omitted $property."
            }
            if ($property -eq 'samples') {
                if (-not (Test-JsonIntegerValue $gpuComposite.$property) -or $gpuComposite.$property -lt 1) {
                    throw "GPU compositor-pass timing returned invalid sample count $($gpuComposite.$property)."
                }
            } elseif (-not (Test-JsonFiniteNumber $gpuComposite.$property) -or $gpuComposite.$property -lt 0) {
                throw "GPU compositor-pass timing returned invalid numeric ${property}: $($gpuComposite.$property)."
            }
        }
        if ($gpuComposite.max_ms -lt $gpuComposite.p95_ms) {
            throw 'GPU compositor-pass timing has max below p95.'
        }
    } elseif ($null -ne $gpuComposite) {
        throw 'GPU compositor-pass timing must be null when timestamp queries are unsupported.'
    }
    $gpuCompletion = $gpuStages.submission_to_completion_elapsed
    if ($null -eq $gpuCompletion) {
        throw 'Surface submission probe omitted GPU submission-to-completion timing.'
    }
    foreach ($property in @('samples', 'p95_ms', 'max_ms')) {
        if ($gpuCompletion.PSObject.Properties.Name -notcontains $property) {
            throw "GPU submission-to-completion timing omitted $property."
        }
        if ($property -eq 'samples') {
            if (-not (Test-JsonIntegerValue $gpuCompletion.$property) -or
                $gpuCompletion.$property -lt 0) {
                throw "GPU submission-to-completion timing returned invalid unsigned integer ${property}: $($gpuCompletion.$property)."
            }
        } elseif (-not (Test-JsonFiniteNumber $gpuCompletion.$property) -or
            $gpuCompletion.$property -lt 0) {
            throw "GPU submission-to-completion timing returned invalid numeric ${property}: $($gpuCompletion.$property)."
        }
    }
    if ($gpuCompletion.max_ms -lt $gpuCompletion.p95_ms) {
        throw 'GPU submission-to-completion timing has max below p95.'
    }
    if ($gpuCompletion.samples -lt 1) {
        throw 'Full media smoke did not observe a completed GPU submission.'
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
    $packageStatus.smoke_status = 'passed'
    $packageStatus | ConvertTo-Json | Set-Content -LiteralPath $packageStatusPath -Encoding utf8
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

Get-Item -LiteralPath $packageExePath
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
