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
if ($contract.schema_version -ne 2 -or $contract.artifact_root -ne 'artifacts/media-fixtures' -or @($contract.outputs).Count -ne 2) { throw 'Unsupported AV1 VFR fixture contract.' }
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
function Assert-OutputContract([string]$Path, [object]$Output, [string]$Label) {
    Assert-Equal (Get-Item -LiteralPath $Path).Length $Output.byte_size "$Label size"
    Assert-Equal (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash $Output.sha256 "$Label SHA-256"
    $probeJson = & $ffprobe -v error -show_streams -show_format -show_packets -show_entries 'stream=codec_name,profile,pix_fmt,width,height,has_b_frames,time_base,r_frame_rate,avg_frame_rate:format=format_name,start_time,duration:packet=pts,dts,duration,flags' -of json $Path 2>$null
    if ($LASTEXITCODE -ne 0) { throw "ffprobe failed to inspect the $Label." }
    $probe = $probeJson | ConvertFrom-Json
    Assert-Equal $probe.format.format_name $Output.container "$Label container"
    Assert-Equal $probe.format.start_time $Output.start_time "$Label start time"
    Assert-Equal $probe.format.duration $Output.duration "$Label duration"
    $streams = @($probe.streams)
    if ($streams.Count -ne 1) { throw "$Label expected one AV1 stream, got $($streams.Count)." }
    Assert-Equal $streams[0].codec_name $Output.stream.codec "$Label stream codec"
    foreach ($field in 'profile', 'width', 'height', 'has_b_frames', 'time_base', 'r_frame_rate', 'avg_frame_rate') { Assert-Equal $streams[0].$field $Output.stream.$field "$Label stream $field" }
    Assert-Equal $streams[0].pix_fmt $Output.stream.pixel_format "$Label stream pixel format"
    $packets = @($probe.packets)
    if ($packets.Count -ne $Output.packets.Count) { throw "$Label packet count expected $($Output.packets.Count), got $($packets.Count)." }
    for ($index = 0; $index -lt $packets.Count; $index++) {
        foreach ($field in 'pts', 'dts', 'duration', 'flags') { Assert-Equal $packets[$index].$field $Output.packets[$index].$field "$Label packet $index $field" }
    }
}
function Enter-Av1FixturePublicationMutex {
    $mutex = [Threading.Mutex]::new($false, 'Local\MaelstromAv1VfrFixturePublication')
    try {
        try { $acquired = $mutex.WaitOne(0) }
        catch [Threading.AbandonedMutexException] { $acquired = $true }
        if (-not $acquired) { throw 'Another AV1 VFR fixture preparation is already running.' }
        return $mutex
    } catch {
        $mutex.Dispose()
        throw
    }
}

$fixtureMutex = Enter-Av1FixturePublicationMutex
try {
$inputPaths = @{}
foreach ($fixtureInput in $contract.inputs) { $inputPaths[$fixtureInput.path] = Assert-Input $fixtureInput }
$ivfInputs = @($contract.inputs | Where-Object { $_.path -like '*.ivf' })
$md5Inputs = @($contract.inputs | Where-Object { $_.path -like '*.ivf.md5' })
if ($ivfInputs.Count -ne 1 -or $md5Inputs.Count -ne 1 -or $contract.inputs.Count -ne 2) { throw 'Expected one pinned AOM IVF input and its MD5 manifest.' }
$md5Lines = @(Get-Content -LiteralPath $inputPaths[$md5Inputs[0].path] | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
if ($md5Lines.Count -lt $md5Inputs[0].frame_md5.Count) { throw "AOM MD5 manifest does not contain the required source frames." }
$referenceFrameMd5 = @()
for ($index = 0; $index -lt $md5Inputs[0].frame_md5.Count; $index++) {
    $actualMd5 = ($md5Lines[$index] -split '\s+')[0]
    Assert-Equal $actualMd5 $md5Inputs[0].frame_md5[$index] "AOM frame MD5 $index"
    $referenceFrameMd5 += $actualMd5
}
if ($referenceFrameMd5.Count -ne 8 -or @($referenceFrameMd5 | Select-Object -Unique).Count -ne 8) { throw 'Expected eight distinct official AOM frame MD5 values.' }

$mkvOutput = @($contract.outputs | Where-Object { $_.id -eq 'matroska' })
$webmOutput = @($contract.outputs | Where-Object { $_.id -eq 'webm' })
if ($mkvOutput.Count -ne 1 -or $webmOutput.Count -ne 1) { throw 'Expected one separately pinned Matroska output and one separately pinned WebM output.' }
$mkvOutput = $mkvOutput[0]
$webmOutput = $webmOutput[0]
$pts = @($mkvOutput.packets | ForEach-Object { [int]$_.pts })
$timestampExpression = [string]$pts[$pts.Count - 1]
for ($index = $pts.Count - 2; $index -ge 0; $index--) {
    $timestampExpression = "if(eq(N\,$index)\,$($pts[$index])\,$timestampExpression)"
}
$setts = "setts=pts='$timestampExpression':dts='$timestampExpression':duration=33:time_base=1/1000:prescale=1"
$mkvOutputPath = Join-Path $artifactRoot $mkvOutput.path
$webmOutputPath = Join-Path $artifactRoot $webmOutput.path
$temporaryMkvPath = Join-Path $artifactRoot ('.' + [IO.Path]::GetFileNameWithoutExtension($mkvOutputPath) + '.' + [guid]::NewGuid().ToString('N') + '.tmp.mkv')
$temporaryWebmPath = Join-Path $artifactRoot ('.' + [IO.Path]::GetFileNameWithoutExtension($webmOutputPath) + '.' + [guid]::NewGuid().ToString('N') + '.tmp.webm')
$publishNonce = [guid]::NewGuid().ToString('N')
$mkvBackupPath = "$mkvOutputPath.$publishNonce.rollback"
$webmBackupPath = "$webmOutputPath.$publishNonce.rollback"
$publishedMkv = $false
$publishedWebm = $false
$publicationSucceeded = $false
try {
    & $ffmpeg -hide_banner -loglevel error -nostdin -y -i $inputPaths[$ivfInputs[0].path] -map 0:v:0 -frames:v 8 -c:v copy -bsf:v $setts -fflags +bitexact -flags:v +bitexact -map_metadata -1 $temporaryMkvPath
    if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath $temporaryMkvPath -PathType Leaf)) { throw "FFmpeg failed to create the temporary AV1 VFR Matroska fixture." }
    Assert-OutputContract $temporaryMkvPath $mkvOutput 'Matroska output'
    & $ffmpeg -hide_banner -loglevel error -nostdin -y -copyts -i $temporaryMkvPath -map 0:v:0 -c:v copy -fflags +bitexact -flags:v +bitexact -map_metadata -1 -f webm $temporaryWebmPath
    if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath $temporaryWebmPath -PathType Leaf)) { throw "FFmpeg failed to create the temporary AV1 VFR WebM fixture." }
    Assert-OutputContract $temporaryWebmPath $webmOutput 'WebM output'
    # Transactional pair publication: both contracts validate first; an interrupted second move
    # restores the prior pair rather than leaving a mixed-generation fixture set.
    try {
        if (Test-Path -LiteralPath $mkvOutputPath -PathType Leaf) { [IO.File]::Move($mkvOutputPath, $mkvBackupPath, $false) }
        if (Test-Path -LiteralPath $webmOutputPath -PathType Leaf) { [IO.File]::Move($webmOutputPath, $webmBackupPath, $false) }
        [IO.File]::Move($temporaryMkvPath, $mkvOutputPath, $false)
        $publishedMkv = $true
        [IO.File]::Move($temporaryWebmPath, $webmOutputPath, $false)
        $publishedWebm = $true
        $publicationSucceeded = $true
    } catch {
        $publicationError = $_
        $restoreErrors = [Collections.Generic.List[string]]::new()
        foreach ($published in @(
            [pscustomobject]@{ active = $publishedMkv; path = $mkvOutputPath; label = 'Matroska replacement' },
            [pscustomobject]@{ active = $publishedWebm; path = $webmOutputPath; label = 'WebM replacement' }
        )) {
            if ($published.active -and (Test-Path -LiteralPath $published.path -PathType Leaf)) {
                try { Remove-Item -LiteralPath $published.path -Force }
                catch { $restoreErrors.Add("Could not remove $($published.label): $($_.Exception.Message)") }
            }
        }
        foreach ($backup in @(
            [pscustomobject]@{ path = $mkvBackupPath; target = $mkvOutputPath; label = 'Matroska backup' },
            [pscustomobject]@{ path = $webmBackupPath; target = $webmOutputPath; label = 'WebM backup' }
        )) {
            if (Test-Path -LiteralPath $backup.path -PathType Leaf) {
                try { [IO.File]::Move($backup.path, $backup.target, $false) }
                catch { $restoreErrors.Add("Could not restore $($backup.label) from $($backup.path): $($_.Exception.Message)") }
            }
        }
        if ($restoreErrors.Count -ne 0) {
            throw "AV1 VFR pair publication failed and rollback is incomplete. Recovery files were preserved. Publication error: $($publicationError.Exception.Message) Rollback errors: $($restoreErrors -join '; ')"
        }
        throw $publicationError
    }
} finally {
    if (Test-Path -LiteralPath $temporaryMkvPath -PathType Leaf) { Remove-Item -LiteralPath $temporaryMkvPath -Force }
    if (Test-Path -LiteralPath $temporaryWebmPath -PathType Leaf) { Remove-Item -LiteralPath $temporaryWebmPath -Force }
    if ($publicationSucceeded) {
        if (Test-Path -LiteralPath $mkvBackupPath -PathType Leaf) { Remove-Item -LiteralPath $mkvBackupPath -Force }
        if (Test-Path -LiteralPath $webmBackupPath -PathType Leaf) { Remove-Item -LiteralPath $webmBackupPath -Force }
    }
}
Write-Output "AV1 VFR fixtures: PASS ($mkvOutputPath; $webmOutputPath)"
} finally {
    $fixtureMutex.ReleaseMutex()
    $fixtureMutex.Dispose()
}
