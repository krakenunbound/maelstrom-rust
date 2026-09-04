#requires -Version 7.0
[CmdletBinding()]
param(
    [ValidateNotNullOrEmpty()]
    [string]$FfmpegRoot = $env:FFMPEG_DIR,
    [string]$ArtifactRoot,
    [switch]$ManifestOnly,
    [switch]$ManifestCoverageContractFixture,
    [switch]$IncludeRealCorpus
)

$ErrorActionPreference = 'Stop'
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
if ([string]::IsNullOrWhiteSpace($ArtifactRoot)) {
    $ArtifactRoot = Join-Path $repoRoot 'artifacts\media-fixtures'
}
$manifestPath = Join-Path $repoRoot 'fixtures\media\manifest.json'
$manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
if ($manifest.schema_version -ne 3 -or [string]::IsNullOrWhiteSpace($manifest.artifact_root)) {
    throw 'Unsupported or incomplete media fixture manifest.'
}
if ($manifest.artifact_root -ne 'artifacts/media-fixtures') { throw 'Unexpected fixture artifact root.' }
if ($ManifestCoverageContractFixture -and -not $ManifestOnly) {
    throw '-ManifestCoverageContractFixture is permitted only with -ManifestOnly.'
}
$seenIds = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
$seenPaths = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
foreach ($fixture in $manifest.fixtures) {
    foreach ($field in 'id', 'path', 'source', 'provenance', 'generation_recipe', 'sha256', 'byte_size', 'container', 'duration_seconds', 'expected') {
        if ($null -eq $fixture.$field -or [string]::IsNullOrWhiteSpace([string]$fixture.$field)) { throw "Fixture is missing $field." }
    }
    if (-not $seenIds.Add([string]$fixture.id) -or -not $seenPaths.Add([string]$fixture.path)) { throw "Duplicate fixture id or path: $($fixture.id)." }
    if ([IO.Path]::IsPathRooted($fixture.path) -or $fixture.path -match '(^|[\\/])\.\.([\\/]|$)') { throw "Unsafe fixture path: $($fixture.path)." }
    if ($fixture.source -ne 'generated') { throw "Unsupported fixture source: $($fixture.source)." }
    if ($fixture.expected -notin @('success', 'ffprobe_failure')) { throw "Unknown expected behavior: $($fixture.expected)." }
    if ($fixture.expected -eq 'success' -and -not ($fixture.PSObject.Properties.Name -contains 'video' -or $fixture.PSObject.Properties.Name -contains 'audio')) { throw "Success fixture needs audio or video metadata: $($fixture.id)." }
    if ($fixture.sha256 -notmatch '^[A-F0-9]{64}$') { throw "Fixture hash is not SHA-256: $($fixture.id)." }
    if ([int64]$fixture.byte_size -lt 1) { throw "Fixture byte size must be positive: $($fixture.id)." }
    if ($fixture.PSObject.Properties.Name -contains 'video') {
        foreach ($field in 'codec', 'rate', 'width', 'height', 'gop', 'has_b_frames', 'keyframe_positions') { if ($null -eq $fixture.video.$field -or [string]::IsNullOrWhiteSpace([string]$fixture.video.$field)) { throw "Video fixture is missing ${field}: $($fixture.id)." } }
        if ([int]$fixture.video.width -lt 1 -or [int]$fixture.video.height -lt 1 -or [int]$fixture.video.gop -lt 1) { throw "Video dimensions and GOP must be positive: $($fixture.id)." }
        if ([int]$fixture.video.has_b_frames -lt 0) { throw "Video FFprobe has_b_frames value must be non-negative: $($fixture.id)." }
        foreach ($field in @('profile', 'pixel_format')) {
            if ($fixture.video.PSObject.Properties.Name -contains $field -and [string]::IsNullOrWhiteSpace([string]$fixture.video.$field)) { throw "Video ${field} contract must not be empty: $($fixture.id)." }
        }
        $keyframePositions = @($fixture.video.keyframe_positions | ForEach-Object { [int]$_ })
        if ($keyframePositions.Count -lt 1 -or $keyframePositions[0] -ne 0) { throw "Video keyframe positions must start at zero: $($fixture.id)." }
        for ($index = 1; $index -lt $keyframePositions.Count; $index++) {
            if ($keyframePositions[$index] -le $keyframePositions[$index - 1]) { throw "Video keyframe positions must be strictly increasing: $($fixture.id)." }
        }
        if ([string]$fixture.video.rate -notmatch '^(\d+)/(\d+)$' -or [int64]$Matches[1] -lt 1 -or [int64]$Matches[2] -lt 1) { throw "Video rate must be a positive rational: $($fixture.id)." }
        if (-not ($fixture.video.PSObject.Properties.Name -contains 'timing')) { throw "Video fixture is missing timing metadata: $($fixture.id)." }
        if ($fixture.video.timing.vfr -isnot [bool]) { throw "Video timing VFR flag must be Boolean: $($fixture.id)." }
        if ($fixture.video.timing.reordered -isnot [bool]) { throw "Video timing reordered flag must be Boolean: $($fixture.id)." }
        if ($fixture.video.PSObject.Properties.Name -contains 'picture_types') {
            $pictureTypes = @($fixture.video.picture_types | ForEach-Object { [string]$_ })
            if ($pictureTypes.Count -lt 1 -or @($pictureTypes | Where-Object { $_ -notmatch '^[IPB]$' }).Count -ne 0) { throw "Video picture types are invalid: $($fixture.id)." }
        }
        if ($fixture.video.timing.vfr) {
            foreach ($field in 'frame_count', 'first_pts_us', 'last_pts_us', 'expected_gap_us', 'pts_sha256') {
                if ($null -eq $fixture.video.timing.$field -or [string]::IsNullOrWhiteSpace([string]$fixture.video.timing.$field)) { throw "VFR video timing is missing ${field}: $($fixture.id)." }
            }
            if ([int]$fixture.video.timing.frame_count -lt 3 -or [int64]$fixture.video.timing.first_pts_us -lt 0 -or [int64]$fixture.video.timing.last_pts_us -le [int64]$fixture.video.timing.first_pts_us) { throw "VFR video timing bounds are invalid: $($fixture.id)." }
            $distinctExpectedGaps = @($fixture.video.timing.expected_gap_us | ForEach-Object { [int64]$_ } | Select-Object -Unique)
            if ($distinctExpectedGaps.Count -lt 2 -or @($distinctExpectedGaps | Where-Object { $_ -le 0 }).Count -ne 0) { throw "VFR video needs at least two positive expected gaps: $($fixture.id)." }
            if ([string]$fixture.video.timing.pts_sha256 -notmatch '^[A-F0-9]{64}$') { throw "VFR PTS hash is not SHA-256: $($fixture.id)." }
        }
        if ($fixture.video.timing.reordered -and [string]$fixture.video.timing.packet_pts_sha256 -notmatch '^[A-F0-9]{64}$') { throw "Reordered video packet PTS hash is not SHA-256: $($fixture.id)." }
    }
    if ($fixture.PSObject.Properties.Name -contains 'audio') {
        foreach ($field in 'codec', 'sample_rate', 'channels', 'layout') { if ($null -eq $fixture.audio.$field -or [string]::IsNullOrWhiteSpace([string]$fixture.audio.$field)) { throw "Audio fixture is missing ${field}: $($fixture.id)." } }
        if ([int]$fixture.audio.sample_rate -lt 1 -or [int]$fixture.audio.channels -lt 1) { throw "Audio rate and channel count must be positive: $($fixture.id)." }
    }
}
if ($ManifestCoverageContractFixture) {
    # Test the real coverage assertion against an in-memory-only incomplete view.
    $manifest.fixtures = @($manifest.fixtures | Where-Object {
        -not ($_.PSObject.Properties.Name -contains 'audio') -or [int]$_.audio.channels -le 2
    })
}

function Assert-ManifestAudioCoverage([object[]]$Fixtures) {
    $audioFixtures = @($Fixtures | Where-Object { $_.PSObject.Properties.Name -contains 'audio' })
    $hasMono = @($audioFixtures | Where-Object {
        [int]$_.audio.channels -eq 1 -and [string]$_.audio.layout -eq 'mono'
    }).Count -gt 0
    $hasStereo = @($audioFixtures | Where-Object {
        [int]$_.audio.channels -eq 2 -and [string]$_.audio.layout -eq 'stereo'
    }).Count -gt 0
    $hasMultichannel = @($audioFixtures | Where-Object {
        [int]$_.audio.channels -gt 2 -and
        -not [string]::IsNullOrWhiteSpace([string]$_.audio.layout) -and
        [string]$_.audio.layout -notin @('mono', 'stereo')
    }).Count -gt 0
    if (-not $hasMono -or -not $hasStereo -or -not $hasMultichannel) {
        throw 'Manifest audio coverage requires exact mono (1 channel), exact stereo (2 channels), and a declared multichannel (>2 channels, non-mono/stereo layout) fixture.'
    }
}

Assert-ManifestAudioCoverage @($manifest.fixtures)
if ($ManifestOnly) { Write-Output 'Media fixture manifest schema: PASS'; exit 0 }

if ([string]::IsNullOrWhiteSpace($FfmpegRoot)) { throw 'Pass -FfmpegRoot with an absolute FFmpeg bundle root or set FFMPEG_DIR.' }
if (-not [IO.Path]::IsPathFullyQualified($FfmpegRoot)) { throw 'FFmpeg root must be an absolute path.' }
$ffmpegRootPath = (Resolve-Path -LiteralPath $FfmpegRoot).Path
$ffprobe = Join-Path $ffmpegRootPath 'bin\ffprobe.exe'
if (-not (Test-Path -LiteralPath $ffprobe -PathType Leaf)) { throw "Expected ffprobe.exe below $ffmpegRootPath\\bin." }
$ffprobeVersion = & $ffprobe -hide_banner -version 2>&1
if ($LASTEXITCODE -ne 0 -or $ffprobeVersion[0] -notmatch '^ffprobe version n?8\.1(?:[.\s-]|$)') { throw "Media fixtures require the pinned FFmpeg 8.1 bundle: $ffmpegRootPath" }
$artifactPath = [IO.Path]::GetFullPath($ArtifactRoot, $repoRoot)
$expectedArtifactPath = Join-Path $repoRoot 'artifacts\media-fixtures'
if (-not [string]::Equals($artifactPath, $expectedArtifactPath, [StringComparison]::OrdinalIgnoreCase)) { throw "Fixture artifacts must be the ignored local artifact directory: $expectedArtifactPath" }

function Assert-Equal([object]$Actual, [object]$Expected, [string]$Label) { if ([string]$Actual -ne [string]$Expected) { throw "$Label expected $Expected, got $Actual." } }
function Get-Sha256Hex([string]$Text) {
    $sha256 = [Security.Cryptography.SHA256]::Create()
    try { return ([Convert]::ToHexString($sha256.ComputeHash([Text.Encoding]::UTF8.GetBytes($Text)))) }
    finally { $sha256.Dispose() }
}
function Convert-PtsToMicroseconds([int64]$Pts, [int64]$TimeBaseNumerator, [int64]$TimeBaseDenominator) {
    $microseconds = ([decimal]$Pts * [decimal]$TimeBaseNumerator * 1000000) / [decimal]$TimeBaseDenominator
    return [int64][Math]::Round($microseconds, 0, [MidpointRounding]::AwayFromZero)
}
foreach ($fixture in $manifest.fixtures) {
    $path = Join-Path $artifactPath $fixture.path
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "Missing fixture: $path" }
    Assert-Equal (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash $fixture.sha256 "Hash for $($fixture.id)"
    Assert-Equal (Get-Item -LiteralPath $path).Length $fixture.byte_size "Byte size for $($fixture.id)"
    $json = & $ffprobe -v error -show_streams -show_format -of json $path 2>$null
    $probeExit = $LASTEXITCODE
    if ($fixture.expected -eq 'ffprobe_failure') { if ($probeExit -eq 0) { throw "Expected ffprobe to fail: $($fixture.id)" }; continue }
    if ($probeExit -ne 0) { throw "ffprobe failed: $($fixture.id)" }
    $probe = $json | ConvertFrom-Json
    Assert-Equal $probe.format.format_name $fixture.container "Container for $($fixture.id)"
    $duration = [double]$probe.format.duration
    if ([Math]::Abs($duration - [double]$fixture.duration_seconds) -gt 0.01) { throw "Duration for $($fixture.id) expected $($fixture.duration_seconds), got $duration." }
    if ($fixture.PSObject.Properties.Name -contains 'video') {
        $video = @($probe.streams | Where-Object codec_type -eq 'video')[0]
        if ($null -eq $video) { throw "Missing video stream: $($fixture.id)" }
        Assert-Equal $video.codec_name $fixture.video.codec "Video codec for $($fixture.id)"
        if ($fixture.video.PSObject.Properties.Name -contains 'profile') {
            Assert-Equal $video.profile $fixture.video.profile "Video profile for $($fixture.id)"
        }
        if ($fixture.video.PSObject.Properties.Name -contains 'pixel_format') {
            Assert-Equal $video.pix_fmt $fixture.video.pixel_format "Video pixel format for $($fixture.id)"
        }
        if (-not $fixture.video.timing.vfr) {
            Assert-Equal $video.r_frame_rate $fixture.video.rate "Video rate for $($fixture.id)"
        }
        Assert-Equal $video.width $fixture.video.width "Video width for $($fixture.id)"
        Assert-Equal $video.height $fixture.video.height "Video height for $($fixture.id)"
        Assert-Equal $video.has_b_frames $fixture.video.has_b_frames "Video FFprobe has_b_frames contract for $($fixture.id)"
        $frameJson = & $ffprobe -v error -select_streams v:0 -show_frames -show_entries frame=key_frame,pict_type,pts -of json $path 2>$null
        if ($LASTEXITCODE -ne 0) { throw "ffprobe frame scan failed: $($fixture.id)" }
        $frames = @((($frameJson | ConvertFrom-Json).frames) | Where-Object { $null -ne $_.pts })
        $keyPositions = @($frames | ForEach-Object -Begin { $index = 0 } -Process { $current = $index; $index++; if ([int]$_.key_frame -eq 1) { $current } })
        $expectedKeyPositions = @($fixture.video.keyframe_positions | ForEach-Object { [int]$_ })
        if ($keyPositions.Count -ne $expectedKeyPositions.Count) { throw "GOP keyframe count for $($fixture.id) expected $($expectedKeyPositions.Count), got $($keyPositions.Count)." }
        for ($index = 0; $index -lt $expectedKeyPositions.Count; $index++) {
            if ($keyPositions[$index] -ne $expectedKeyPositions[$index]) { throw "GOP keyframe $index for $($fixture.id) expected $($expectedKeyPositions[$index]), got $($keyPositions[$index])." }
        }
        if ($fixture.video.PSObject.Properties.Name -contains 'picture_types') {
            $actualPictureTypes = @($frames | ForEach-Object { [string]$_.pict_type })
            $expectedPictureTypes = @($fixture.video.picture_types | ForEach-Object { [string]$_ })
            if ($actualPictureTypes.Count -ne $expectedPictureTypes.Count) { throw "Picture type count for $($fixture.id) expected $($expectedPictureTypes.Count), got $($actualPictureTypes.Count)." }
            for ($index = 0; $index -lt $expectedPictureTypes.Count; $index++) {
                Assert-Equal $actualPictureTypes[$index] $expectedPictureTypes[$index] "Picture type $index for $($fixture.id)"
            }
        }
        if ($fixture.video.timing.vfr) {
            $timeBaseParts = [string]$video.time_base -split '/'
            if ($timeBaseParts.Count -ne 2 -or [int64]$timeBaseParts[0] -lt 1 -or [int64]$timeBaseParts[1] -lt 1) { throw "Invalid video time base: $($fixture.id)" }
            $ptsUs = @($frames | ForEach-Object { Convert-PtsToMicroseconds ([int64]$_.pts) ([int64]$timeBaseParts[0]) ([int64]$timeBaseParts[1]) })
            Assert-Equal $ptsUs.Count $fixture.video.timing.frame_count "VFR frame count for $($fixture.id)"
            Assert-Equal $ptsUs[0] $fixture.video.timing.first_pts_us "VFR first PTS for $($fixture.id)"
            Assert-Equal $ptsUs[$ptsUs.Count - 1] $fixture.video.timing.last_pts_us "VFR last PTS for $($fixture.id)"
            $gapsUs = @()
            for ($index = 1; $index -lt $ptsUs.Count; $index++) {
                $gap = [int64]$ptsUs[$index] - [int64]$ptsUs[$index - 1]
                if ($gap -le 0) { throw "VFR PTS are not strictly monotonic at frame ${index}: $($fixture.id)" }
                $gapsUs += $gap
            }
            $actualDistinctGaps = @($gapsUs | Select-Object -Unique)
            if ($actualDistinctGaps.Count -lt 2) { throw "VFR PTS gaps are not irregular: $($fixture.id)" }
            foreach ($expectedGap in @($fixture.video.timing.expected_gap_us | ForEach-Object { [int64]$_ } | Select-Object -Unique)) {
                if ($expectedGap -notin $actualDistinctGaps) { throw "Expected VFR PTS gap $expectedGap us not found: $($fixture.id)" }
            }
            Assert-Equal (Get-Sha256Hex ($ptsUs -join ',')) $fixture.video.timing.pts_sha256 "VFR PTS fingerprint for $($fixture.id)"
            if ($fixture.video.timing.reordered) {
                $packetJson = & $ffprobe -v error -select_streams v:0 -show_packets -show_entries packet=pts,dts -of json $path 2>$null
                if ($LASTEXITCODE -ne 0) { throw "ffprobe packet scan failed: $($fixture.id)" }
                $packets = @((($packetJson | ConvertFrom-Json).packets) | Where-Object { $null -ne $_.pts })
                $packetPtsUs = @($packets | ForEach-Object { Convert-PtsToMicroseconds ([int64]$_.pts) ([int64]$timeBaseParts[0]) ([int64]$timeBaseParts[1]) })
                $nonMonotonic = $false
                for ($index = 1; $index -lt $packetPtsUs.Count; $index++) { if ($packetPtsUs[$index] -lt $packetPtsUs[$index - 1]) { $nonMonotonic = $true; break } }
                if (-not $nonMonotonic) { throw "Reordered packet PTS were monotonic: $($fixture.id)" }
                if (@($packets | Where-Object { $null -ne $_.dts -and [int64]$_.pts -ne [int64]$_.dts }).Count -eq 0) { throw "Reordered packet PTS never differed from DTS: $($fixture.id)" }
                Assert-Equal (Get-Sha256Hex ($packetPtsUs -join ',')) $fixture.video.timing.packet_pts_sha256 "Reordered packet PTS fingerprint for $($fixture.id)"
            }
        }
    }
    if ($fixture.PSObject.Properties.Name -contains 'audio') {
        $audio = @($probe.streams | Where-Object codec_type -eq 'audio')[0]
        if ($null -eq $audio) { throw "Missing audio stream: $($fixture.id)" }
        Assert-Equal $audio.codec_name $fixture.audio.codec "Audio codec for $($fixture.id)"
        Assert-Equal $audio.sample_rate $fixture.audio.sample_rate "Audio rate for $($fixture.id)"
        Assert-Equal $audio.channels $fixture.audio.channels "Audio channels for $($fixture.id)"
        $layout = $audio.channel_layout
        if ([string]::IsNullOrWhiteSpace($layout) -and [int]$audio.channels -eq 1) { $layout = 'mono' }
        Assert-Equal $layout $fixture.audio.layout "Audio layout for $($fixture.id)"
    }
}

if ($IncludeRealCorpus) {
    $corpusRoot = $env:MAELSTROM_REAL_MEDIA_ROOT
    if ([string]::IsNullOrWhiteSpace($corpusRoot)) { throw 'Set MAELSTROM_REAL_MEDIA_ROOT to an explicit local corpus directory.' }
    $resolvedCorpus = (Resolve-Path -LiteralPath $corpusRoot).Path
    $files = Get-ChildItem -LiteralPath $resolvedCorpus -File -Recurse
    if ($files.Count -eq 0) { throw "Real-media corpus is empty: $resolvedCorpus" }
    foreach ($file in $files) { & $ffprobe -v error -show_format -of json $file.FullName *> $null; if ($LASTEXITCODE -ne 0) { throw "Real-media corpus probe failed: $($file.FullName)" } }
    Write-Output "Real-media corpus: PASS ($($files.Count) files)"
}
Write-Output "Media fixtures: PASS ($($manifest.fixtures.Count) fixtures)"
