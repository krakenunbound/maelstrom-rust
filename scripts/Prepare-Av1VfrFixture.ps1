#requires -Version 7.0
[CmdletBinding()]
param(
    [switch]$DownloadInputs
)

$ErrorActionPreference = 'Stop'
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$artifactRoot = Join-Path $repoRoot 'artifacts\media-fixtures'
$approvedFfmpegRoot = Join-Path $repoRoot '.deps\ffmpeg-project-8.1'
$metadataPath = Join-Path $repoRoot 'fixtures\media\av1-vfr-fixture.json'

if (-not (Test-Path -LiteralPath $metadataPath -PathType Leaf)) { throw "Missing fixture contract: $metadataPath" }
$contract = Get-Content -LiteralPath $metadataPath -Raw | ConvertFrom-Json
if ($contract.schema_version -ne 1 -or $contract.artifact_root -ne 'artifacts/media-fixtures') { throw 'Unsupported AV1 VFR fixture contract.' }
if (-not [string]::Equals((Resolve-Path -LiteralPath $approvedFfmpegRoot).Path, $approvedFfmpegRoot, [StringComparison]::OrdinalIgnoreCase)) {
    throw "AV1 VFR fixture requires the approved FFmpeg 8.1 root: $approvedFfmpegRoot"
}
$ffmpeg = Join-Path $approvedFfmpegRoot 'bin\ffmpeg.exe'
$ffprobe = Join-Path $approvedFfmpegRoot 'bin\ffprobe.exe'
if (-not (Test-Path -LiteralPath $ffmpeg -PathType Leaf) -or -not (Test-Path -LiteralPath $ffprobe -PathType Leaf)) { throw "Expected ffmpeg.exe and ffprobe.exe below $approvedFfmpegRoot\bin." }
$version = & $ffmpeg -hide_banner -version 2>&1
if ($LASTEXITCODE -ne 0 -or $version[0] -notmatch '^ffmpeg version n?8\.1(?:[.\s-]|$)') { throw "AV1 VFR fixture requires FFmpeg 8.1: $approvedFfmpegRoot" }

New-Item -ItemType Directory -Force -Path $artifactRoot | Out-Null
function Assert-Equal([object]$Actual, [object]$Expected, [string]$Label) {
    if ([string]$Actual -cne [string]$Expected) { throw "$Label expected $Expected, got $Actual." }
}
function Assert-FileContract([string]$Path, [object]$FixtureInput) {
    Assert-Equal (Get-Item -LiteralPath $Path).Length $FixtureInput.byte_size "Size for $($FixtureInput.path)"
    Assert-Equal (Get-FileHash -LiteralPath $Path -Algorithm SHA1).Hash $FixtureInput.sha1 "SHA-1 for $($FixtureInput.path)"
    Assert-Equal (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash $FixtureInput.sha256 "SHA-256 for $($FixtureInput.path)"
}
function Assert-Input([object]$FixtureInput) {
    $path = Join-Path $artifactRoot $FixtureInput.path
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        if (-not $DownloadInputs) { throw "Missing local input: $path. Re-run with -DownloadInputs to fetch the pinned AOM test inputs." }
        $temporaryPath = Join-Path $artifactRoot ('.' + [IO.Path]::GetFileName($path) + '.' + [guid]::NewGuid().ToString('N') + '.download')
        try {
            Invoke-WebRequest -Uri $FixtureInput.url -OutFile $temporaryPath
            Assert-FileContract $temporaryPath $FixtureInput
            [IO.File]::Move($temporaryPath, $path, $false)
        } finally {
            if (Test-Path -LiteralPath $temporaryPath -PathType Leaf) { Remove-Item -LiteralPath $temporaryPath -Force }
        }
    }
    Assert-FileContract $path $FixtureInput
    return $path
}

$seedPath = Assert-Input $contract.inputs[0]
$md5Path = Assert-Input $contract.inputs[1]
$md5Lines = @(Get-Content -LiteralPath $md5Path | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
if ($md5Lines.Count -ne $contract.inputs[1].frame_md5.Count) { throw 'Unexpected AOM frame-MD5 line count.' }
for ($index = 0; $index -lt $md5Lines.Count; $index++) {
    $actualMd5 = ($md5Lines[$index] -split '\s+')[0]
    Assert-Equal $actualMd5 $contract.inputs[1].frame_md5[$index] "AOM frame MD5 $index"
}

$pts = @($contract.output.packets | ForEach-Object { [int]$_.pts })
$timestampExpression = [string]$pts[$pts.Count - 1]
for ($index = $pts.Count - 2; $index -ge 0; $index--) {
    $timestampExpression = "if(eq(N\,$index)\,$($pts[$index])\,$timestampExpression)"
}
$setts = "setts=pts='$timestampExpression':dts='$timestampExpression':duration=33:time_base=1/1000:prescale=1"
$outputPath = Join-Path $artifactRoot $contract.output.path
$temporaryOutputPath = Join-Path $artifactRoot ('.' + [IO.Path]::GetFileNameWithoutExtension($outputPath) + '.' + [guid]::NewGuid().ToString('N') + '.tmp.mkv')
try {
    & $ffmpeg -hide_banner -loglevel error -nostdin -y -stream_loop 3 -i $seedPath -map 0:v:0 -frames:v 8 -c:v copy -bsf:v $setts -fflags +bitexact -flags:v +bitexact -map_metadata -1 $temporaryOutputPath
    if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath $temporaryOutputPath -PathType Leaf)) { throw "FFmpeg failed to create the temporary AV1 VFR fixture." }

    Assert-Equal (Get-Item -LiteralPath $temporaryOutputPath).Length $contract.output.byte_size 'Output size'
    Assert-Equal (Get-FileHash -LiteralPath $temporaryOutputPath -Algorithm SHA256).Hash $contract.output.sha256 'Output SHA-256'
    $probeJson = & $ffprobe -v error -show_streams -show_format -show_packets -show_entries 'stream=codec_name,profile,pix_fmt,width,height,has_b_frames,time_base,r_frame_rate,avg_frame_rate:format=format_name,start_time,duration:packet=pts,dts,duration,flags' -of json $temporaryOutputPath 2>$null
    if ($LASTEXITCODE -ne 0) { throw 'ffprobe failed to inspect the AV1 VFR fixture.' }
    $probe = $probeJson | ConvertFrom-Json
    Assert-Equal $probe.format.format_name $contract.output.container 'Output container'
    Assert-Equal $probe.format.start_time $contract.output.start_time 'Output start time'
    Assert-Equal $probe.format.duration $contract.output.duration 'Output duration'
    $streams = @($probe.streams)
    if ($streams.Count -ne 1) { throw "Expected one AV1 stream, got $($streams.Count)." }
    Assert-Equal $streams[0].codec_name $contract.output.stream.codec 'Stream codec'
    foreach ($field in 'profile', 'width', 'height', 'has_b_frames', 'time_base', 'r_frame_rate', 'avg_frame_rate') { Assert-Equal $streams[0].$field $contract.output.stream.$field "Stream $field" }
    Assert-Equal $streams[0].pix_fmt $contract.output.stream.pixel_format 'Stream pixel format'
    $packets = @($probe.packets)
    if ($packets.Count -ne $contract.output.packets.Count) { throw "Packet count expected $($contract.output.packets.Count), got $($packets.Count)." }
    for ($index = 0; $index -lt $packets.Count; $index++) {
        foreach ($field in 'pts', 'dts', 'duration', 'flags') { Assert-Equal $packets[$index].$field $contract.output.packets[$index].$field "Packet $index $field" }
    }
    [IO.File]::Move($temporaryOutputPath, $outputPath, $true)
} finally {
    if (Test-Path -LiteralPath $temporaryOutputPath -PathType Leaf) { Remove-Item -LiteralPath $temporaryOutputPath -Force }
}
Write-Output "AV1 VFR fixture: PASS ($outputPath)"
