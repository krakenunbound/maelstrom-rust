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

function Add-BigEndianUInt32([Collections.Generic.List[byte]]$Bytes, [uint32]$Value) {
    $Bytes.Add([byte](($Value -shr 24) -band 0xFF))
    $Bytes.Add([byte](($Value -shr 16) -band 0xFF))
    $Bytes.Add([byte](($Value -shr 8) -band 0xFF))
    $Bytes.Add([byte]($Value -band 0xFF))
}

function Get-Crc32([byte[]]$Bytes) {
    [uint32]$crc = 4294967295
    foreach ($value in $Bytes) {
        $crc = $crc -bxor [uint32]$value
        for ($bit = 0; $bit -lt 8; $bit++) {
            if (($crc -band 1) -ne 0) { $crc = ($crc -shr 1) -bxor [uint32]3988292384 }
            else { $crc = $crc -shr 1 }
        }
    }
    return $crc -bxor [uint32]4294967295
}

function Add-PngChunk([Collections.Generic.List[byte]]$Png, [string]$Type, [byte[]]$Data) {
    $typeBytes = [Text.Encoding]::ASCII.GetBytes($Type)
    $crcBytes = [Collections.Generic.List[byte]]::new()
    $crcBytes.AddRange($typeBytes)
    $crcBytes.AddRange($Data)
    Add-BigEndianUInt32 $Png ([uint32]$Data.Length)
    $Png.AddRange($typeBytes)
    $Png.AddRange($Data)
    Add-BigEndianUInt32 $Png (Get-Crc32 $crcBytes.ToArray())
}

function New-DeterministicRgbaPng([string]$Target) {
    $width = 160
    $height = 90
    $raw = [Collections.Generic.List[byte]]::new()
    for ($y = 0; $y -lt $height; $y++) {
        $raw.Add(0) # PNG filter type: None
        for ($x = 0; $x -lt $width; $x++) {
            $raw.Add([byte][Math]::Floor(255 * $x / ($width - 1)))
            $raw.Add([byte][Math]::Floor(255 * $y / ($height - 1)))
            $raw.Add([byte][Math]::Floor(255 * ($x + $y) / (($width - 1) + ($height - 1))))
            $raw.Add([byte][Math]::Floor(255 * $x / ($width - 1)))
        }
    }
    $compressed = [IO.MemoryStream]::new()
    $zlib = [IO.Compression.ZLibStream]::new($compressed, [IO.Compression.CompressionLevel]::NoCompression, $true)
    try { $zlib.Write($raw.ToArray(), 0, $raw.Count) }
    finally { $zlib.Dispose() }
    $png = [Collections.Generic.List[byte]]::new()
    $png.AddRange([byte[]](0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A))
    $header = [Collections.Generic.List[byte]]::new()
    Add-BigEndianUInt32 $header $width
    Add-BigEndianUInt32 $header $height
    $header.AddRange([byte[]](8, 6, 0, 0, 0)) # 8-bit RGBA, no interlacing
    Add-PngChunk $png 'IHDR' $header.ToArray()
    Add-PngChunk $png 'IDAT' $compressed.ToArray()
    Add-PngChunk $png 'IEND' ([byte[]]::new(0))
    [IO.File]::WriteAllBytes($Target, $png.ToArray())
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

# Four frames keep 4K coverage bounded while retaining an observable two-frame GOP.
$fourKPath = Join-Path $outputPath 'bars-4k-mpeg4-24.mp4'
Invoke-FixtureFfmpeg @(
    '-f', 'lavfi', '-i', 'testsrc2=size=3840x2160:rate=24', '-frames:v', '4', '-an',
    '-c:v', 'mpeg4', '-q:v', '8', '-g', '2', '-bf', '0', '-pix_fmt', 'yuv420p',
    '-fflags', '+bitexact', '-flags:v', '+bitexact', '-map_metadata', '-1', '-movflags', '+faststart'
) $fourKPath

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

# Keep each 5.1 channel distinguishable so channel order and layout handling can
# be verified from a single deterministic PCM source (FL, FR, FC, LFE, BL, BR).
$surroundWavPath = Join-Path $outputPath 'surround-51-pcm-48k.wav'
Invoke-FixtureFfmpeg @(
    '-f', 'lavfi', '-i', 'aevalsrc=sin(2*PI*220*t)|sin(2*PI*330*t)|sin(2*PI*440*t)|sin(2*PI*55*t)|sin(2*PI*550*t)|sin(2*PI*660*t):sample_rate=48000:channel_layout=5.1', '-t', '1',
    '-c:a', 'pcm_s16le', '-ar', '48000'
) $surroundWavPath

# A compact still-image fixture with transparent, translucent, and opaque regions.
$imagePath = Join-Path $outputPath 'alpha-pattern-rgba.png'
New-DeterministicRgbaPng $imagePath

$corruptPath = Join-Path $outputPath 'truncated-header.bin'
[IO.File]::WriteAllBytes($corruptPath, [byte[]](0x00, 0x00, 0x00, 0x0C, 0x66, 0x74, 0x79, 0x70, 0x6D, 0x70, 0x34, 0x32))

& (Join-Path $PSScriptRoot 'Test-MediaFixtures.ps1') -FfmpegRoot $ffmpegRootPath -ArtifactRoot $outputPath
