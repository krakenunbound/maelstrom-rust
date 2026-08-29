[CmdletBinding()]
param(
    [ValidateNotNullOrEmpty()]
    [string]$FfmpegRoot = $env:FFMPEG_DIR,
    [string]$ArtifactRoot = (Join-Path $PSScriptRoot '..\artifacts\media-fixtures'),
    [switch]$ManifestOnly,
    [switch]$IncludeRealCorpus
)

$ErrorActionPreference = 'Stop'
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$manifestPath = Join-Path $repoRoot 'fixtures\media\manifest.json'
$manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
if ($manifest.schema_version -ne 1 -or [string]::IsNullOrWhiteSpace($manifest.artifact_root)) {
    throw 'Unsupported or incomplete media fixture manifest.'
}
if ($manifest.artifact_root -ne 'artifacts/media-fixtures') { throw 'Unexpected fixture artifact root.' }
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
        foreach ($field in 'codec', 'rate', 'width', 'height', 'gop') { if ($null -eq $fixture.video.$field -or [string]::IsNullOrWhiteSpace([string]$fixture.video.$field)) { throw "Video fixture is missing ${field}: $($fixture.id)." } }
        if ([int]$fixture.video.width -lt 1 -or [int]$fixture.video.height -lt 1 -or [int]$fixture.video.gop -lt 1) { throw "Video dimensions and GOP must be positive: $($fixture.id)." }
        if ([string]$fixture.video.rate -notmatch '^(\d+)/(\d+)$' -or [int64]$Matches[1] -lt 1 -or [int64]$Matches[2] -lt 1) { throw "Video rate must be a positive rational: $($fixture.id)." }
    }
    if ($fixture.PSObject.Properties.Name -contains 'audio') {
        foreach ($field in 'codec', 'sample_rate', 'channels', 'layout') { if ($null -eq $fixture.audio.$field -or [string]::IsNullOrWhiteSpace([string]$fixture.audio.$field)) { throw "Audio fixture is missing ${field}: $($fixture.id)." } }
        if ([int]$fixture.audio.sample_rate -lt 1 -or [int]$fixture.audio.channels -lt 1) { throw "Audio rate and channel count must be positive: $($fixture.id)." }
    }
}
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
        Assert-Equal $video.r_frame_rate $fixture.video.rate "Video rate for $($fixture.id)"
        Assert-Equal $video.width $fixture.video.width "Video width for $($fixture.id)"
        Assert-Equal $video.height $fixture.video.height "Video height for $($fixture.id)"
        Assert-Equal $video.has_b_frames 0 "Video GOP/B-frame contract for $($fixture.id)"
        $keyFrames = @(& $ffprobe -v error -select_streams v:0 -show_frames -show_entries frame=key_frame -of csv=p=0 $path 2>$null)
        if ($LASTEXITCODE -ne 0) { throw "ffprobe frame scan failed: $($fixture.id)" }
        $keyPositions = @($keyFrames | ForEach-Object -Begin { $index = 0 } -Process { $value = $_; $current = $index; $index++; if ($value -eq '1') { $current } })
        $expectedKeyPositions = [Collections.Generic.List[int]]::new()
        for ($position = 0; $position -lt $keyFrames.Count; $position += [int]$fixture.video.gop) {
            $expectedKeyPositions.Add($position)
        }
        if ($keyPositions.Count -ne $expectedKeyPositions.Count) { throw "GOP keyframe count for $($fixture.id) expected $($expectedKeyPositions.Count), got $($keyPositions.Count)." }
        for ($index = 0; $index -lt $expectedKeyPositions.Count; $index++) {
            if ($keyPositions[$index] -ne $expectedKeyPositions[$index]) { throw "GOP keyframe $index for $($fixture.id) expected $($expectedKeyPositions[$index]), got $($keyPositions[$index])." }
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
