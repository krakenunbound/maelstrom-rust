#requires -Version 7.0
[CmdletBinding()]
param(
    [string]$ReportPath,
    [switch]$IncludeAdapterInventory
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Restore-EnvironmentValue([string]$Name, $Value) {
    if ($null -eq $Value) { Remove-Item "Env:$Name" -ErrorAction SilentlyContinue }
    else { Set-Item "Env:$Name" $Value }
}

function Write-AtomicJson([string]$Path, $Value) {
    $temporaryPath = Join-Path ([IO.Path]::GetDirectoryName($Path)) ('.' + [IO.Path]::GetFileName($Path) + '.' + [guid]::NewGuid().ToString('N') + '.tmp')
    try {
        [IO.File]::WriteAllText($temporaryPath, ($Value | ConvertTo-Json -Depth 12), [Text.UTF8Encoding]::new($false))
        [IO.File]::Move($temporaryPath, $Path, $true)
    } finally {
        if (Test-Path -LiteralPath $temporaryPath -PathType Leaf) { Remove-Item -LiteralPath $temporaryPath -Force }
    }
}

function Get-GitText([string[]]$Arguments) {
    $text = & $git -C $repoRoot @Arguments 2>$null
    if ($LASTEXITCODE -ne 0) { throw "git $($Arguments -join ' ') failed." }
    return ($text -join "`n").Trim()
}

function Add-Log([System.Text.StringBuilder]$Log, [string]$Text) {
    [void]$Log.AppendLine($Text)
}

function Write-BoundedUtf8Log([string]$Path, [string]$Text) {
    $maximumBytes = 1048576
    $encoding = [Text.UTF8Encoding]::new($false)
    $bytes = $encoding.GetBytes($Text)
    if ($bytes.Length -gt $maximumBytes) {
        $banner = $encoding.GetBytes("[truncated to the final log bytes; retained file is at most $maximumBytes UTF-8 bytes]`r`n")
        $tailLength = $maximumBytes - $banner.Length
        $start = $bytes.Length - $tailLength
        while ($start -lt $bytes.Length -and (($bytes[$start] -band 0xC0) -eq 0x80)) { $start++ }
        $tail = [byte[]]::new($bytes.Length - $start)
        [Array]::Copy($bytes, $start, $tail, 0, $tail.Length)
        $bytes = $banner + $tail
    }
    if ($bytes.Length -gt $maximumBytes) { throw "Bounded log exceeded $maximumBytes UTF-8 bytes." }
    [IO.File]::WriteAllBytes($Path, $bytes)
}

function Assert-PinnedFfmpegBundle([string]$BundleRoot) {
    $bundleBin = Join-Path $BundleRoot 'bin'
    $ffmpeg = Join-Path $bundleBin 'ffmpeg.exe'
    $ffprobe = Join-Path $bundleBin 'ffprobe.exe'
    $buildManifest = Join-Path $BundleRoot 'BUILD-MANIFEST.txt'
    $buildChecksums = Join-Path $BundleRoot 'BUILD-SHA256SUMS.txt'
    if (-not (Test-Path -LiteralPath $ffmpeg -PathType Leaf) -or -not (Test-Path -LiteralPath $ffprobe -PathType Leaf)) { throw 'The FFmpeg bundle must contain bin\ffmpeg.exe and bin\ffprobe.exe.' }
    if (-not (Test-Path -LiteralPath $buildManifest -PathType Leaf) -or -not (Test-Path -LiteralPath $buildChecksums -PathType Leaf)) { throw 'Qualification requires the project-built FFmpeg manifest and checksum inventory.' }
    $manifestText = Get-Content -LiteralPath $buildManifest -Raw
    if ($manifestText -notmatch 'FFmpeg commit: 9047fa1b084f76b1b4d065af2d743df1b40dfb56' -or $manifestText -notmatch 'nv-codec-headers commit: 1889e62e2d35ff7aa9baca2bceb14f053785e6f1' -or $manifestText -notmatch 'oneVPL commit: 2274efcd3672b43297ef774f332e1fed6781381c') { throw "The FFmpeg build manifest does not match Maelstrom's pinned source revisions." }
    $bundlePrefix = $BundleRoot.TrimEnd([IO.Path]::DirectorySeparatorChar) + [IO.Path]::DirectorySeparatorChar
    $verifiedFiles = 0
    foreach ($line in Get-Content -LiteralPath $buildChecksums) {
        if ($line -notmatch '^([0-9a-f]{64})  (.+)$') { throw "Malformed FFmpeg checksum entry: $line" }
        $expectedHash = $Matches[1].ToUpperInvariant()
        $relativePath = $Matches[2].Replace('/', [IO.Path]::DirectorySeparatorChar)
        $artifact = [IO.Path]::GetFullPath((Join-Path $BundleRoot $relativePath))
        if (-not $artifact.StartsWith($bundlePrefix, [StringComparison]::OrdinalIgnoreCase) -or -not (Test-Path -LiteralPath $artifact -PathType Leaf)) { throw "FFmpeg checksum target is missing or unsafe: $relativePath" }
        if ((Get-FileHash -LiteralPath $artifact -Algorithm SHA256).Hash -cne $expectedHash) { throw "FFmpeg checksum mismatch: $relativePath" }
        $verifiedFiles++
    }
    $configuration = (& $ffmpeg -hide_banner -version 2>&1 | Out-String)
    if ($LASTEXITCODE -ne 0 -or $configuration -notmatch 'ffmpeg version n?8\.1' -or $configuration -notmatch '--enable-shared') { throw 'Qualification requires the frozen FFmpeg 8.1 shared-library line.' }
    if ($configuration -match '--enable-gpl' -or $configuration -match '--enable-nonfree') { throw 'Qualification refuses GPL/nonfree FFmpeg.' }
    return [ordered]@{ path = $ffmpeg; ffprobe_path = $ffprobe; version_line = ($configuration -split "`r?`n")[0]; sha256 = (Get-FileHash -LiteralPath $ffmpeg -Algorithm SHA256).Hash; manifest_path = $buildManifest; checksums_path = $buildChecksums; verified_checksum_file_count = $verifiedFiles; shared_bundle_verified = $true }
}

function Assert-Av1CliReferencePixels([string]$FfmpegPath, [string]$FixturePath) {
    $expectedMd5 = @('1998867ce2f47e15728862d6b55de0b4', '48e9c8687a16b488ba1f7c49cb1f78fc', '1998867ce2f47e15728862d6b55de0b4', '48e9c8687a16b488ba1f7c49cb1f78fc', '1998867ce2f47e15728862d6b55de0b4', '48e9c8687a16b488ba1f7c49cb1f78fc', '1998867ce2f47e15728862d6b55de0b4', '48e9c8687a16b488ba1f7c49cb1f78fc')
    $paths = @(
        [ordered]@{ label = 'D3D11VA'; decoder = 'av1'; hwaccel = 'd3d11va' },
        [ordered]@{ label = 'DXVA2'; decoder = 'av1'; hwaccel = 'dxva2' },
        [ordered]@{ label = 'NVIDIA CUVID'; decoder = 'av1_cuvid'; hwaccel = $null },
        [ordered]@{ label = 'Intel Quick Sync'; decoder = 'av1_qsv'; hwaccel = $null }
    )
    $evidence = @()
    foreach ($path in $paths) {
        $arguments = @('-v', 'error', '-nostdin')
        if ($null -ne $path.hwaccel) { $arguments += @('-hwaccel', $path.hwaccel) }
        $arguments += @('-c:v', $path.decoder, '-i', $FixturePath, '-map', '0:v:0', '-an', '-frames:v', '8', '-pix_fmt', 'yuv420p', '-f', 'framemd5', '-')
        $output = & $FfmpegPath @arguments 2>&1
        $exitCode = $LASTEXITCODE
        $md5 = @($output | ForEach-Object {
            $line = $_.ToString()
            if ($line -match '(?i),\s*([0-9a-f]{32})$') { $Matches[1].ToLowerInvariant() }
        })
        $matchesOfficialReference = $exitCode -eq 0 -and $md5.Count -eq $expectedMd5.Count -and ([string]::Join(',', $expectedMd5) -ceq [string]::Join(',', $md5))
        $evidence += [ordered]@{ label = $path.label; decoder = $path.decoder; hwaccel = $path.hwaccel; exit_code = $exitCode; frame_md5 = $md5; matches_official_aom_reference = $matchesOfficialReference }
        if (-not $matchesOfficialReference) { throw "AV1 FFmpeg CLI pixel preflight did not match the official AOM reference: $($path.label) ($($path.decoder))." }
    }
    return $evidence
}

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$cargo = 'C:\Users\The Kraken\.cargo\bin\cargo.exe'
$git = 'D:\PythonStuff\Git\cmd\git.exe'
$ffmpegRoot = Join-Path $repoRoot '.deps\ffmpeg-project-8.1'
$libclangRoot = Join-Path $repoRoot '.deps\libclang-bindgen'
$artifactRoot = [IO.Path]::GetFullPath((Join-Path $repoRoot 'artifacts\phase1-hardware-vfr'))
$fixtureRoot = [IO.Path]::GetFullPath((Join-Path $repoRoot 'artifacts\phase1-multisource'))
if ([string]::IsNullOrWhiteSpace($ReportPath)) { $ReportPath = Join-Path $artifactRoot 'phase1-hardware-vfr.schema1.json' }
$resolvedReportPath = if ([IO.Path]::IsPathRooted($ReportPath)) { [IO.Path]::GetFullPath($ReportPath) } else { [IO.Path]::GetFullPath((Join-Path $repoRoot $ReportPath)) }
if (-not [string]::Equals([IO.Path]::GetDirectoryName($resolvedReportPath), $artifactRoot, [StringComparison]::OrdinalIgnoreCase) -or [IO.Path]::GetExtension($resolvedReportPath) -ine '.json') {
    throw "Report output must be a JSON file directly inside $artifactRoot"
}

$fixtureContracts = @(
    [ordered]@{ codec = 'h264'; path = (Join-Path $fixtureRoot 'codec-vfr-h264-bt709-1080p-hardware.mp4'); sha256 = '503B39F6C101F8395B49AD424711357DC317C2CAEFCFCC9E5F795A0D46CDCAA6' },
    [ordered]@{ codec = 'hevc_main10'; path = (Join-Path $fixtureRoot 'codec-vfr-hevc-main10-bt709-1080p-hardware.mp4'); sha256 = '1AF892D8C40634E354A05FD80A446298C5501914D2E39ECD641D792B6538C486' },
    [ordered]@{ codec = 'av1_main'; path = (Join-Path $repoRoot 'artifacts\media-fixtures\vfr-av1-aom-shifted.mkv'); sha256 = '6ADB3B081701F13ED7C5EFDC26F092E08D474AE2D9E7840B6C58A2B937A9EC9C' }
)
$tests = @(
    [ordered]@{ name = 'scrub_seek_tests::supplied_windows_d3d11va_h264_vfr_scrub_matches_independent_cli_reference'; backend = 'D3D11VA'; codec = 'h264'; label = 'D3D11VA supplied H.264 VFR Windows D3D11VA' },
    [ordered]@{ name = 'scrub_seek_tests::supplied_windows_dxva2_h264_vfr_scrub_matches_independent_cli_reference'; backend = 'DXVA2'; codec = 'h264'; label = 'DXVA2 supplied H.264 VFR Windows DXVA2' },
    [ordered]@{ name = 'scrub_seek_tests::supplied_windows_cuvid_h264_vfr_scrub_matches_independent_cli_reference'; backend = 'NVIDIA CUVID'; codec = 'h264'; label = 'CUVID supplied H.264 VFR NVIDIA CUVID' },
    [ordered]@{ name = 'scrub_seek_tests::supplied_windows_qsv_h264_vfr_scrub_matches_independent_cli_reference'; backend = 'Intel Quick Sync'; codec = 'h264'; label = 'QSV supplied H.264 VFR Intel Quick Sync' },
    [ordered]@{ name = 'scrub_seek_tests::supplied_windows_d3d11va_hevc_vfr_scrub_matches_independent_cli_reference'; backend = 'D3D11VA'; codec = 'hevc_main10'; label = 'D3D11VA supplied HEVC VFR Windows D3D11VA' },
    [ordered]@{ name = 'scrub_seek_tests::supplied_windows_dxva2_hevc_vfr_scrub_matches_independent_cli_reference'; backend = 'DXVA2'; codec = 'hevc_main10'; label = 'DXVA2 supplied HEVC VFR Windows DXVA2' },
    [ordered]@{ name = 'scrub_seek_tests::supplied_windows_cuvid_hevc_vfr_scrub_matches_independent_cli_reference'; backend = 'NVIDIA CUVID'; codec = 'hevc_main10'; label = 'CUVID supplied HEVC VFR NVIDIA CUVID' },
    [ordered]@{ name = 'scrub_seek_tests::supplied_windows_qsv_hevc_vfr_scrub_matches_independent_cli_reference'; backend = 'Intel Quick Sync'; codec = 'hevc_main10'; label = 'QSV supplied HEVC VFR Intel Quick Sync' },
    [ordered]@{ name = 'scrub_seek_tests::supplied_windows_d3d11va_av1_vfr_scrub_matches_independent_cli_reference'; backend = 'D3D11VA'; codec = 'av1_main'; label = 'D3D11VA supplied AV1 VFR Windows D3D11VA' },
    [ordered]@{ name = 'scrub_seek_tests::supplied_windows_dxva2_av1_vfr_scrub_matches_independent_cli_reference'; backend = 'DXVA2'; codec = 'av1_main'; label = 'DXVA2 supplied AV1 VFR Windows DXVA2' },
    [ordered]@{ name = 'scrub_seek_tests::supplied_windows_cuvid_av1_vfr_scrub_matches_independent_cli_reference'; backend = 'NVIDIA CUVID'; codec = 'av1_main'; label = 'CUVID supplied AV1 VFR NVIDIA CUVID' },
    [ordered]@{ name = 'scrub_seek_tests::supplied_windows_qsv_av1_vfr_scrub_matches_independent_cli_reference'; backend = 'Intel Quick Sync'; codec = 'av1_main'; label = 'QSV supplied AV1 VFR Intel Quick Sync' }
)

$saved = @{}
foreach ($name in @('PATH', 'FFMPEG_DIR', 'LIBCLANG_PATH', 'MAELSTROM_HARDWARE_H264_VFR_TEST_MEDIA', 'MAELSTROM_HEVC_VFR_TEST_MEDIA', 'MAELSTROM_AV1_VFR_TEST_MEDIA')) {
    $saved[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
}
$failure = $null
$log = [System.Text.StringBuilder]::new()
$runResults = @()
$fixtureEvidence = @()
$sourceStart = $null
$sourceEnd = $null
$sourceTrackedDirtyAtStart = $null
$sourceTrackedDirtyAtEnd = $null
$ffmpegIdentity = $null
$av1CliPixelEvidence = $null
$adapterInventory = $null
$startedUtc = [DateTime]::UtcNow.ToString('o')
$runMutex = [Threading.Mutex]::new($false, 'Local\MaelstromRustPhase1HardwareVfrLock')
$runMutexHeld = $false
$logWriteFailure = $null
$reportWriteFailure = $null

try {
    try {
        $runMutexHeld = $runMutex.WaitOne()
    } catch [Threading.AbandonedMutexException] {
        $runMutexHeld = $true
    }
    if (-not $runMutexHeld) { throw 'Could not acquire the Phase 1 hardware VFR artifact lock.' }
    New-Item -ItemType Directory -Force -Path $artifactRoot | Out-Null
    $sourceStart = Get-GitText @('rev-parse', 'HEAD')
    if (-not (Test-Path -LiteralPath $git -PathType Leaf)) { throw "Missing required Git executable: $git" }
    $sourceTrackedDirtyAtStart = -not [string]::IsNullOrEmpty((Get-GitText @('status', '--porcelain', '--untracked-files=no')))
    foreach ($path in @($cargo, (Join-Path $libclangRoot 'libclang.dll'))) {
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "Missing required repo-local qualification dependency: $path" }
    }
    $ffmpegIdentity = Assert-PinnedFfmpegBundle $ffmpegRoot
    foreach ($fixture in $fixtureContracts) {
        if (-not (Test-Path -LiteralPath $fixture.path -PathType Leaf)) { throw "Missing required existing fixture: $($fixture.path)" }
        $item = Get-Item -LiteralPath $fixture.path
        $actualHash = (Get-FileHash -LiteralPath $fixture.path -Algorithm SHA256).Hash
        if ($actualHash -cne $fixture.sha256) { throw "Fixture SHA-256 does not match the documented contract: $($fixture.path)" }
        $fixtureEvidence += [ordered]@{ codec = $fixture.codec; path = $fixture.path; size_bytes = [int64]$item.Length; sha256 = $actualHash; documented_sha256 = $fixture.sha256; reused_existing = $true }
    }
    $av1CliPixelEvidence = Assert-Av1CliReferencePixels $ffmpegIdentity.path $fixtureContracts[2].path
    if ($IncludeAdapterInventory) {
        $adapterInventory = @(Get-CimInstance Win32_VideoController | ForEach-Object { [ordered]@{ name = $_.Name; driver_version = $_.DriverVersion; pnp_device_id = $_.PNPDeviceID } })
    }
    $env:FFMPEG_DIR = $ffmpegRoot
    $env:LIBCLANG_PATH = $libclangRoot
    $env:PATH = (Join-Path $ffmpegRoot 'bin') + [IO.Path]::PathSeparator + $libclangRoot + [IO.Path]::PathSeparator + $saved['PATH']
    $env:MAELSTROM_HARDWARE_H264_VFR_TEST_MEDIA = $fixtureContracts[0].path
    $env:MAELSTROM_HEVC_VFR_TEST_MEDIA = $fixtureContracts[1].path
    $env:MAELSTROM_AV1_VFR_TEST_MEDIA = $fixtureContracts[2].path
    Push-Location -LiteralPath $repoRoot
    try {
        foreach ($test in $tests) {
            Add-Log $log ("=== {0} ===" -f $test.name)
            $output = & $cargo test -p nle-decode --release $test.name -- --ignored --exact --test-threads=1 --nocapture 2>&1
            $exitCode = $LASTEXITCODE
            $text = $output -join "`n"
            Add-Log $log $text
            $proof64 = [regex]::IsMatch($text, '(?m)^' + [regex]::Escape($test.label) + ' 64x48: 8 VFR boundaries, 19 exact CLI-reference seek cases$')
            $proof1080 = [regex]::IsMatch($text, '(?m)^' + [regex]::Escape($test.label) + ' 1920x1080: 8 VFR boundaries, 19 exact CLI-reference seek cases$')
            $namedDecoderReopen = $null
            $proofNamedDecoderReopen = $true
            if ($test.backend -eq 'Intel Quick Sync') {
                $proofPattern = '(?m)^' + [regex]::Escape($test.label) + ' (?<width>64|1920)x(?<height>48|1080): named decoder reopen: (?<samples>\d+) samples, total (?<total>\d+\.\d{3}) ms, mean (?<mean>\d+\.\d{3}) ms, max (?<max>\d+\.\d{3}) ms$'
                $proofMatches = [regex]::Matches($text, $proofPattern)
                $proofNamedDecoderReopen = $proofMatches.Count -eq 2
                $namedDecoderReopen = @()
                foreach ($proofMatch in $proofMatches) {
                    $samples = [UInt64]$proofMatch.Groups['samples'].Value
                    $proofNamedDecoderReopen = $proofNamedDecoderReopen -and $samples -eq 7
                    $namedDecoderReopen += [ordered]@{
                        output_size = @([int]$proofMatch.Groups['width'].Value, [int]$proofMatch.Groups['height'].Value)
                        samples = $samples
                        total_ms = [double]::Parse($proofMatch.Groups['total'].Value, [Globalization.CultureInfo]::InvariantCulture)
                        mean_ms = [double]::Parse($proofMatch.Groups['mean'].Value, [Globalization.CultureInfo]::InvariantCulture)
                        max_ms = [double]::Parse($proofMatch.Groups['max'].Value, [Globalization.CultureInfo]::InvariantCulture)
                    }
                }
                $proofNamedDecoderReopen = $proofNamedDecoderReopen -and @($namedDecoderReopen | Where-Object { $_.output_size[0] -eq 64 -and $_.output_size[1] -eq 48 }).Count -eq 1 -and @($namedDecoderReopen | Where-Object { $_.output_size[0] -eq 1920 -and $_.output_size[1] -eq 1080 }).Count -eq 1
            }
            $passed = $exitCode -eq 0 -and $proof64 -and $proof1080 -and $proofNamedDecoderReopen -and $text -match 'test result: ok\. 1 passed; 0 failed;'
            $runResults += [ordered]@{ test = $test.name; backend = $test.backend; codec = $test.codec; exit_code = $exitCode; passed = $passed; output_proves_64x48 = $proof64; output_proves_1920x1080 = $proof1080; output_proves_named_decoder_reopen = $proofNamedDecoderReopen; named_decoder_reopen = $namedDecoderReopen; vfr_boundaries = if ($passed) { 8 } else { $null }; exact_cases_per_size = if ($passed) { 19 } else { $null } }
            if (-not $passed) { throw "Hardware VFR test did not meet its exact-output contract: $($test.name)" }
        }
    } finally { Pop-Location }
} catch {
    $failure = $_.Exception.Message
} finally {
    try {
        $sourceEnd = Get-GitText @('rev-parse', 'HEAD')
        $sourceTrackedDirtyAtEnd = -not [string]::IsNullOrEmpty((Get-GitText @('status', '--porcelain', '--untracked-files=no')))
    } catch { if ($null -eq $failure) { $failure = $_.Exception.Message } }
    $logPath = Join-Path $artifactRoot 'phase1-hardware-vfr.log'
    if ($runMutexHeld) {
        try { Write-BoundedUtf8Log $logPath $log.ToString() } catch { $logWriteFailure = $_.Exception.Message }
        if ($null -ne $logWriteFailure -and $null -eq $failure) { $failure = "Retained log write failed: $logWriteFailure" }
        try {
            $report = [ordered]@{
            schema_version = 1
            status = if ($null -eq $failure) { 'passed' } else { 'failed' }
            failure = $failure
            started_utc = $startedUtc
            finished_utc = [DateTime]::UtcNow.ToString('o')
            source = [ordered]@{ start_commit = $sourceStart; end_commit = $sourceEnd; tracked_dirty_at_start = $sourceTrackedDirtyAtStart; tracked_dirty_at_end = $sourceTrackedDirtyAtEnd }
            authoritative = ($null -eq $failure -and $null -ne $sourceStart -and $sourceStart -eq $sourceEnd -and $sourceTrackedDirtyAtStart -eq $false -and $sourceTrackedDirtyAtEnd -eq $false)
            ffmpeg = $ffmpegIdentity
            av1_cli_pixel_preflight = $av1CliPixelEvidence
            fixtures = $fixtureEvidence
            tests = $runResults
            test_count = $runResults.Count
            passed_test_count = @($runResults | Where-Object { $_.passed }).Count
            exact_cases_total = if ($null -eq $failure -and $runResults.Count -eq 12) { 456 } else { $null }
            backend_apis = @('D3D11VA', 'DXVA2', 'NVIDIA CUVID', 'Intel Quick Sync')
            codecs = @('h264', 'hevc_main10', 'av1_main')
            output_sizes = @(@(64, 48), @(1920, 1080))
            retained_log = [IO.Path]::GetFullPath($logPath)
            log_write_failure = $logWriteFailure
            limits = [ordered]@{
                physical_gpu_claim = $false
                gui_or_editor_launched = $false
                export_parity_claim = $false
                note = 'D3D11VA, DXVA2, NVIDIA CUVID, and Intel Quick Sync are decoder-path qualifications on this host; this harness does not prove physical-adapter identity, GUI presentation, scanout, export parity, or other machines.'
            }
            adapter_inventory = if ($IncludeAdapterInventory) { [ordered]@{ inventory_only = $true; adapters = $adapterInventory } } else { $null }
            }
            Write-AtomicJson $resolvedReportPath $report
        } catch { $reportWriteFailure = $_.Exception.Message }
    }
    # Always restore the caller environment and dispose the mutex, even when an
    # abandoned mutex is recovered before this run safely publishes evidence.
    foreach ($name in $saved.Keys) { Restore-EnvironmentValue $name $saved[$name] }
    if ($runMutexHeld) { $runMutex.ReleaseMutex() }
    $runMutex.Dispose()
}
if ($null -ne $reportWriteFailure) { throw "Phase 1 hardware VFR qualification could not publish its report: $reportWriteFailure" }
if ($null -ne $failure) { throw "Phase 1 hardware VFR qualification failed; preserved report: $resolvedReportPath. $failure" }
Write-Host "Phase 1 hardware VFR qualification: PASS ($resolvedReportPath; 12 tests; 456 exact cases)"
