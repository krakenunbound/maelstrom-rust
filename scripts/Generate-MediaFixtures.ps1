#requires -Version 7.0
[CmdletBinding()]
param(
    [ValidateNotNullOrEmpty()]
    [string]$FfmpegRoot = $env:FFMPEG_DIR,
    [string]$OutputRoot
)

$ErrorActionPreference = 'Stop'
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
if ([string]::IsNullOrWhiteSpace($OutputRoot)) {
    $OutputRoot = Join-Path $repoRoot 'artifacts\media-fixtures'
}
if ([string]::IsNullOrWhiteSpace($FfmpegRoot)) {
    throw 'Pass -FfmpegRoot with an absolute FFmpeg bundle root or set FFMPEG_DIR.'
}
if (-not [IO.Path]::IsPathFullyQualified($FfmpegRoot)) {
    throw 'FFmpeg root must be an absolute path.'
}
$ffmpegRootPath = (Resolve-Path -LiteralPath $FfmpegRoot).Path
$ffmpeg = Join-Path $ffmpegRootPath 'bin\ffmpeg.exe'
$ffprobe = Join-Path $ffmpegRootPath 'bin\ffprobe.exe'
if (-not (Test-Path -LiteralPath $ffmpeg -PathType Leaf) -or -not (Test-Path -LiteralPath $ffprobe -PathType Leaf)) {
    throw "Expected ffmpeg.exe and ffprobe.exe below $ffmpegRootPath\\bin."
}
$ffmpegVersion = & $ffmpeg -hide_banner -version 2>&1
if ($LASTEXITCODE -ne 0 -or $ffmpegVersion[0] -notmatch '^ffmpeg version n?8\.1(?:[.\s-]|$)') {
    throw "Media fixtures require the pinned FFmpeg 8.1 bundle: $ffmpegRootPath"
}

$outputPath = [IO.Path]::GetFullPath($OutputRoot, $repoRoot)
$expectedOutputPath = Join-Path $repoRoot 'artifacts\media-fixtures'
if (-not [string]::Equals($outputPath, $expectedOutputPath, [StringComparison]::OrdinalIgnoreCase)) {
    throw "Fixture output must be the ignored local artifact directory: $expectedOutputPath"
}
New-Item -ItemType Directory -Force -Path $outputPath | Out-Null

function Invoke-FixtureFfmpeg([string[]]$Arguments, [string]$Target) {
    & $ffmpeg -hide_banner -loglevel error -nostdin -y @Arguments $Target
    if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath $Target -PathType Leaf)) {
        throw "FFmpeg failed to generate $Target."
    }
}

# This mirrors the deterministic package acceptance source while keeping the fixture short.
$avPath = Join-Path $outputPath 'bars-aac-2997.mp4'
Invoke-FixtureFfmpeg @(
    '-f', 'lavfi', '-i', 'testsrc2=size=320x180:rate=30000/1001',
    '-f', 'lavfi', '-i', 'sine=frequency=1000:sample_rate=48000',
    '-t', '2', '-map', '0:v:0', '-map', '1:a:0', '-c:v', 'mpeg4', '-q:v', '8', '-g', '30',
    '-c:a', 'aac', '-ac', '2', '-ar', '48000', '-movflags', '+faststart',
    '-metadata', 'creation_time=1970-01-01T00:00:00Z'
) $avPath

# Select source frames at deliberately uneven millisecond timestamps.  The select
# filter preserves the 1/1000-second input PTS and -fps_mode vfr prevents the
# muxer from filling those gaps with CFR duplicates.
$vfrPath = Join-Path $outputPath 'vfr-irregular-mpeg4.mp4'
Invoke-FixtureFfmpeg @(
    '-f', 'lavfi', '-i', 'testsrc2=size=160x90:rate=1000',
    '-vf', "select='eq(n,0)+eq(n,40)+eq(n,110)+eq(n,150)+eq(n,240)'",
    '-frames:v', '5', '-fps_mode', 'vfr', '-an', '-c:v', 'mpeg4', '-q:v', '8', '-g', '5', '-bf', '0',
    '-movflags', '+faststart', '-metadata', 'creation_time=1970-01-01T00:00:00Z'
) $vfrPath

# MPEG-TS preserves decode packet order, while the selected 24 fps source frames
# retain deliberately irregular presentation timing. B-frames make the packet PTS
# order differ from presentation order.
$reorderedVfrPath = Join-Path $outputPath 'vfr-reordered-mpeg2.ts'
Invoke-FixtureFfmpeg @(
    '-f', 'lavfi', '-i', 'testsrc2=size=160x90:rate=24',
    '-vf', "select='eq(n,0)+eq(n,1)+eq(n,3)+eq(n,4)+eq(n,6)+eq(n,8)+eq(n,11)+eq(n,12)'",
    '-frames:v', '8', '-fps_mode', 'vfr', '-an', '-c:v', 'mpeg2video', '-q:v', '8', '-g', '8', '-bf', '2',
    '-fflags', '+bitexact', '-flags:v', '+bitexact', '-map_metadata', '-1', '-muxdelay', '0'
) $reorderedVfrPath

# This MP4 deliberately combines a nonzero presentation origin with irregular
# selected 30 fps frames and B-frame packet reordering. It exercises consumers
# that must retain source-time PTS rather than normalize them to a local origin.
$shiftedReorderedVfrPath = Join-Path $outputPath 'vfr-reordered-shifted-mpeg4.mp4'
Invoke-FixtureFfmpeg @(
    '-f', 'lavfi', '-i', 'testsrc2=size=320x180:rate=30',
    '-vf', "select='eq(n,0)+eq(n,1)+eq(n,3)+eq(n,4)+eq(n,6)+eq(n,8)+eq(n,11)+eq(n,12)',setpts=PTS+3/TB",
    '-frames:v', '8', '-fps_mode', 'vfr', '-an', '-c:v', 'mpeg4', '-q:v', '8', '-g', '8', '-bf', '2',
    '-fflags', '+bitexact', '-flags:v', '+bitexact', '-map_metadata', '-1', '-movflags', '+faststart'
) $shiftedReorderedVfrPath

# Intra-frame professional formats with 10-bit 4:2:2 pixels and a nonzero MOV
# presentation origin. Their local frame timing must still begin at zero.
foreach ($codec in @('prores', 'dnxhr')) {
    $encoderOptions = if ($codec -eq 'prores') {
        @('-c:v', 'prores_ks', '-profile:v', '2')
    } else {
        @('-c:v', 'dnxhd', '-profile:v', 'dnxhr_hqx')
    }
    Invoke-FixtureFfmpeg (@(
        '-f', 'lavfi', '-i', 'testsrc2=size=320x180:rate=24',
        '-vf', "select='eq(n,0)+eq(n,1)+eq(n,3)+eq(n,4)+eq(n,6)+eq(n,8)+eq(n,11)+eq(n,12)',setpts=PTS+7/TB",
        '-frames:v', '8', '-fps_mode', 'vfr', '-an'
    ) + $encoderOptions + @(
        '-pix_fmt', 'yuv422p10le', '-fflags', '+bitexact', '-flags:v', '+bitexact', '-map_metadata', '-1'
    )) (Join-Path $outputPath "vfr-$codec-10bit-shifted.mov")
}

$wavPath = Join-Path $outputPath 'mono-pcm-48k.wav'
Invoke-FixtureFfmpeg @(
    '-f', 'lavfi', '-i', 'sine=frequency=440:sample_rate=48000', '-t', '1',
    '-c:a', 'pcm_s16le', '-ac', '1', '-ar', '48000'
) $wavPath

$corruptPath = Join-Path $outputPath 'truncated-header.bin'
[IO.File]::WriteAllBytes($corruptPath, [byte[]](0x00, 0x00, 0x00, 0x0C, 0x66, 0x74, 0x79, 0x70, 0x6D, 0x70, 0x34, 0x32))

& (Join-Path $PSScriptRoot 'Test-MediaFixtures.ps1') -FfmpegRoot $ffmpegRootPath -ArtifactRoot $outputPath
