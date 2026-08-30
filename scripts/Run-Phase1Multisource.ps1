[CmdletBinding()]
param(
    [string]$ReportPath
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

function Test-AbsolutePath([string]$Path) {
    return [IO.Path]::IsPathRooted($Path) -and
        [string]::Equals([IO.Path]::GetFullPath($Path), $Path, [StringComparison]::OrdinalIgnoreCase)
}

function Normalize-ExtendedPath([string]$Path) {
    if ($Path.StartsWith('\\?\')) {
        return $Path.Substring(4)
    }
    return $Path
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

function Assert-JsonIntegerProperty {
    param(
        [Parameter(Mandatory = $true)]$Object,
        [Parameter(Mandatory = $true)][string]$Name,
        [Parameter(Mandatory = $true)][string]$Context
    )
    $property = $Object.PSObject.Properties[$Name]
    if ($null -eq $property -or -not (Test-JsonIntegerValue $property.Value)) {
        throw "$Context omitted or invalidated integer $Name."
    }
}

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$cargoCommand = Get-Command cargo.exe -CommandType Application -ErrorAction Stop
$cargoExecutable = [IO.Path]::GetFullPath($cargoCommand.Source)
$ffmpegRoot = Join-Path $repoRoot '.deps\ffmpeg-project-8.1'
$ffmpeg = Join-Path $ffmpegRoot 'bin\ffmpeg.exe'
$ffprobe = Join-Path $ffmpegRoot 'bin\ffprobe.exe'
$libclangRoot = Join-Path $repoRoot '.deps\libclang-bindgen'
$libclang = Join-Path $libclangRoot 'libclang.dll'
$artifactRoot = Join-Path $repoRoot 'artifacts\phase1-multisource'
$resolvedArtifactRoot = [IO.Path]::GetFullPath($artifactRoot)
if ([string]::IsNullOrWhiteSpace($ReportPath)) {
    $ReportPath = Join-Path $resolvedArtifactRoot 'phase1-multisource.json'
}
$resolvedReportPath = if ([IO.Path]::IsPathRooted($ReportPath)) {
    [IO.Path]::GetFullPath($ReportPath)
} else {
    [IO.Path]::GetFullPath((Join-Path $repoRoot $ReportPath))
}

if (-not [string]::Equals([IO.Path]::GetDirectoryName($resolvedReportPath), $resolvedArtifactRoot, [StringComparison]::OrdinalIgnoreCase)) {
    throw "Report output must be directly inside the ignored artifact directory: $resolvedArtifactRoot"
}
if ([IO.Path]::GetExtension($resolvedReportPath) -ine '.json') {
    throw 'Report output must be a JSON file.'
}
if (-not (Test-Path -LiteralPath $ffmpeg -PathType Leaf) -or -not (Test-Path -LiteralPath $ffprobe -PathType Leaf)) {
    throw "Missing pinned FFmpeg 8.1 binaries below $ffmpegRoot\bin."
}
if (-not (Test-Path -LiteralPath $libclang -PathType Leaf)) {
    throw "Missing local libclang required by native FFmpeg bindings: $libclang"
}
$ffmpegVersion = & $ffmpeg -hide_banner -version 2>&1
if ($LASTEXITCODE -ne 0 -or $ffmpegVersion[0] -notmatch '^ffmpeg version n?8\.1(?:[.\s-]|$)') {
    throw "Phase 1 multisource gate requires the pinned FFmpeg 8.1 bundle: $ffmpegRoot"
}

New-Item -ItemType Directory -Force -Path $resolvedArtifactRoot | Out-Null
$hueDegrees = @(0, 60, 120, 180)
$fixtures = $hueDegrees | ForEach-Object {
    [IO.Path]::GetFullPath((Join-Path $resolvedArtifactRoot ("source-hue-{0:d3}.mp4" -f $_)))
}

$savedPath = $env:PATH
$savedFfmpeg = $env:FFMPEG_DIR
$savedLibclang = $env:LIBCLANG_PATH
$savedFirst = $env:MAELSTROM_TEST_MEDIA
$savedSecond = $env:MAELSTROM_TEST_MEDIA_SECOND
$savedThird = $env:MAELSTROM_TEST_MEDIA_THIRD
$savedFourth = $env:MAELSTROM_TEST_MEDIA_FOURTH
$savedReport = $env:MAELSTROM_PHASE1_MULTISOURCE_REPORT
$runLock = [Threading.Mutex]::new($false, 'Local\MaelstromRustPhase1SustainedFixtureLock')
$runLockAcquired = $false

try {
    if (-not $runLock.WaitOne(0)) { throw 'Another Phase 1 fixture/sustained run owns the exclusive local artifact lock.' }
    $runLockAcquired = $true
    $env:FFMPEG_DIR = $ffmpegRoot
    $env:LIBCLANG_PATH = $libclangRoot
    $env:PATH = (Join-Path $ffmpegRoot 'bin') + [IO.Path]::PathSeparator + $libclangRoot + [IO.Path]::PathSeparator + $savedPath

    for ($index = 0; $index -lt $fixtures.Count; $index++) {
        $fixture = $fixtures[$index]
        Remove-Item -LiteralPath $fixture -Force -ErrorAction SilentlyContinue
        & $ffmpeg -hide_banner -loglevel error -y `
            -f lavfi -i "testsrc2=size=1920x1080:rate=30,hue=h=$($hueDegrees[$index]):s=1" `
            -t 5 -an -c:v mpeg4 -g 30 -q:v 6 -movflags +faststart $fixture
        if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath $fixture -PathType Leaf)) {
            throw "Could not create dynamic 1920x1080 MPEG-4 source fixture: $fixture"
        }
        $probe = & $ffprobe -v error -select_streams v:0 -show_entries stream=codec_name,width,height,r_frame_rate,avg_frame_rate -of default=noprint_wrappers=1 $fixture
        $duration = & $ffprobe -v error -show_entries format=duration -of default=noprint_wrappers=1 $fixture
        if ($LASTEXITCODE -ne 0 -or $probe -notcontains 'codec_name=mpeg4' -or $probe -notcontains 'width=1920' -or $probe -notcontains 'height=1080' -or
            (($probe -notcontains 'r_frame_rate=30/1') -and ($probe -notcontains 'avg_frame_rate=30/1')) -or
            $duration -notcontains 'duration=5.000000') {
            throw "Generated fixture did not satisfy the dynamic 1920x1080 30fps five-second MPEG-4 contract: $fixture"
        }
    }

    Remove-Item -LiteralPath $resolvedReportPath -Force -ErrorAction SilentlyContinue
    $env:MAELSTROM_TEST_MEDIA = $fixtures[0]
    $env:MAELSTROM_TEST_MEDIA_SECOND = $fixtures[1]
    $env:MAELSTROM_TEST_MEDIA_THIRD = $fixtures[2]
    $env:MAELSTROM_TEST_MEDIA_FOURTH = $fixtures[3]
    $env:MAELSTROM_PHASE1_MULTISOURCE_REPORT = $resolvedReportPath
    & $cargoExecutable test -p nle-app --release tests::supplied_media_four_video_layers_decode_independently -- --ignored --exact --test-threads=1
    if ($LASTEXITCODE -ne 0) { throw 'Phase 1 four-source gate failed.' }

    if (-not (Test-Path -LiteralPath $resolvedReportPath -PathType Leaf)) {
        throw 'Phase 1 four-source gate did not write its report.'
    }
    $report = Get-Content -LiteralPath $resolvedReportPath -Raw | ConvertFrom-Json
    foreach ($property in @(
        'schema_version', 'source_count', 'submission_us', 'all_frames_ms',
        'active_sticky_sessions', 'peak_sticky_sessions', 'session_cap',
        'active_foreground_sessions', 'foreground_session_cap',
        'active_background_sessions', 'background_session_cap',
        'live_source_groups', 'source_group_cap', 'live_lane_actors', 'lane_actor_cap', 'retiring_lane_actors',
        'post_drop_active_sessions'
    )) {
        Assert-JsonUnsignedIntegerProperty $report $property 'Phase 1 multisource report'
    }
    Assert-JsonIntegerProperty $report 'requested_source_tick' 'Phase 1 multisource report'
    if ($report.schema_version -ne 1 -or $report.status -ne 'passed' -or $report.source_count -ne 4 -or
        $report.submission_us -ge 20000 -or $report.all_frames_ms -gt 5000 -or
        $report.active_sticky_sessions -ne 5 -or $report.peak_sticky_sessions -ne 5 -or $report.session_cap -ne 8 -or
        $report.active_foreground_sessions -ne 4 -or $report.foreground_session_cap -ne 4 -or
        $report.active_background_sessions -ne 1 -or $report.background_session_cap -ne 4 -or
        $report.live_source_groups -ne 4 -or $report.source_group_cap -ne 4 -or
        $report.live_lane_actors -ne 5 -or $report.lane_actor_cap -ne 8 -or $report.retiring_lane_actors -ne 0 -or
        $report.post_drop_active_sessions -ne 0) {
        throw "Phase 1 multisource report did not prove the required bounded concurrent decode state: $($report | ConvertTo-Json -Compress)"
    }
    if (@($report.decoded_media_ids).Count -ne 4 -or (@($report.decoded_media_ids) -join ',') -ne '1,2,3,4') {
        throw 'Phase 1 multisource report did not decode four independent media IDs in layer order.'
    }
    if (@($report.output_size).Count -ne 2 -or $report.output_size[0] -ne 1920 -or $report.output_size[1] -ne 1080) {
        throw 'Phase 1 multisource report did not use explicit Full-quality 1920x1080 output.'
    }
    if ($report.requested_source_tick -ne 1500000 -or @($report.decoded_source_ticks).Count -ne 4 -or
        @($report.decoded_source_ticks | Where-Object { -not (Test-JsonIntegerValue $_) -or $_ -lt $report.requested_source_tick }).Count -ne 0) {
        throw 'Phase 1 multisource report did not retain four source ticks at or after the requested mid-GOP tick.'
    }
    $backends = @($report.observed_decoder_backends)
    if ($backends.Count -lt 1 -or @($backends | Where-Object { $_ -isnot [string] -or [string]::IsNullOrWhiteSpace($_) }).Count -ne 0) {
        throw 'Phase 1 multisource report omitted applicable decoder backend identity.'
    }
    $sources = @($report.sources)
    if ($sources.Count -ne 4) { throw 'Phase 1 multisource report omitted source evidence.' }
    for ($index = 0; $index -lt $sources.Count; $index++) {
        $source = $sources[$index]
        $sourcePath = Normalize-ExtendedPath $source.path
        if ($source.path -isnot [string] -or -not (Test-AbsolutePath $source.path) -or
            -not [string]::Equals([IO.Path]::GetFullPath($sourcePath), $fixtures[$index], [StringComparison]::OrdinalIgnoreCase) -or
            -not (Test-JsonIntegerValue $source.size_bytes) -or $source.size_bytes -ne (Get-Item -LiteralPath $fixtures[$index]).Length -or
            $source.size_bytes -lt 1) {
            throw "Phase 1 multisource source evidence is invalid at index $index."
        }
    }
    Write-Host "Phase 1 multisource: PASS ($resolvedReportPath; submission $($report.submission_us) us; all frames $($report.all_frames_ms) ms)"
}
finally {
    Restore-EnvironmentValue -Name 'PATH' -Value $savedPath
    Restore-EnvironmentValue -Name 'FFMPEG_DIR' -Value $savedFfmpeg
    Restore-EnvironmentValue -Name 'LIBCLANG_PATH' -Value $savedLibclang
    Restore-EnvironmentValue -Name 'MAELSTROM_TEST_MEDIA' -Value $savedFirst
    Restore-EnvironmentValue -Name 'MAELSTROM_TEST_MEDIA_SECOND' -Value $savedSecond
    Restore-EnvironmentValue -Name 'MAELSTROM_TEST_MEDIA_THIRD' -Value $savedThird
    Restore-EnvironmentValue -Name 'MAELSTROM_TEST_MEDIA_FOURTH' -Value $savedFourth
    Restore-EnvironmentValue -Name 'MAELSTROM_PHASE1_MULTISOURCE_REPORT' -Value $savedReport
    if ($runLockAcquired) { $runLock.ReleaseMutex() }
    $runLock.Dispose()
}
