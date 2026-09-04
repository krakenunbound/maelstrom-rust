#requires -Version 7.0
[CmdletBinding()]
param(
    [ValidateNotNullOrEmpty()]
    [string]$FfmpegRoot = $env:FFMPEG_DIR,
    [string]$ArtifactRoot,
    [switch]$ManifestOnly,
    [switch]$ManifestCoverageContractFixture,
    [switch]$ManifestImageContractFixture,
    [switch]$Manifest4kCoverageContractFixture,
    [switch]$ManifestLocalCorpusContractSchemaFixture,
    [switch]$ManifestLocalCorpusDurationSchemaFixture,
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
if (($ManifestCoverageContractFixture -or $ManifestImageContractFixture -or $Manifest4kCoverageContractFixture -or $ManifestLocalCorpusContractSchemaFixture -or $ManifestLocalCorpusDurationSchemaFixture) -and -not $ManifestOnly) {
    throw 'Manifest contract fixtures are permitted only with -ManifestOnly.'
}
if ($ManifestImageContractFixture) {
    $imageFixtures = @($manifest.fixtures | Where-Object { $_.PSObject.Properties.Name -contains 'image' })
    if ($imageFixtures.Count -lt 1) { throw 'Manifest image contract fixture requires at least one image fixture.' }
    $imageFixtures[0].image.pixel_format = $null
}
if ($ManifestLocalCorpusContractSchemaFixture) {
    $manifest.real_media_corpus.required_local_fixtures[0].video.picture_type_counts.B = 'invalid'
}
if ($ManifestLocalCorpusDurationSchemaFixture) {
    $manifest.real_media_corpus.required_local_fixtures[0].duration_seconds = '5'
}

function Test-LocalContractInteger([object]$Value, [int64]$Minimum, [int64]$Maximum) {
    if ($null -eq $Value) { return $false }
    $integerTypes = @([sbyte], [byte], [int16], [uint16], [int], [uint32], [int64], [uint64])
    if ($Value.GetType() -notin $integerTypes) { return $false }
    $integer = [int64]$Value
    return $integer -ge $Minimum -and $integer -le $Maximum
}
function Test-LocalContractFiniteNumber([object]$Value) {
    if ($null -eq $Value) { return $false }
    $numericTypes = @([sbyte], [byte], [int16], [uint16], [int], [uint32], [int64], [uint64], [single], [double], [decimal])
    if ($Value.GetType() -notin $numericTypes) { return $false }
    $number = [double]$Value
    return [double]::IsFinite($number)
}
function Assert-LocalCorpusContractSchema([object]$Corpus) {
    foreach ($field in 'environment_variable', 'redistribution', 'usage') {
        if ($null -eq $Corpus.$field -or [string]::IsNullOrWhiteSpace([string]$Corpus.$field)) { throw "Real-media corpus is missing $field." }
    }
    if ($null -eq $Corpus.required_local_fixtures -or @($Corpus.required_local_fixtures).Count -lt 1) { throw 'Real-media corpus is missing required_local_fixtures.' }
    $seenIds = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
    $seenNames = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    foreach ($fixture in @($Corpus.required_local_fixtures)) {
        foreach ($field in 'id', 'filename', 'source', 'provenance', 'generation_recipe', 'local_only', 'permission_policy', 'sha256', 'byte_size', 'container', 'duration_seconds', 'video', 'encoder_intent') {
            if ($null -eq $fixture.$field -or [string]::IsNullOrWhiteSpace([string]$fixture.$field)) { throw "Required local fixture is missing $field." }
        }
        if ($fixture.source -ne 'local_only' -or $fixture.local_only -isnot [bool] -or -not $fixture.local_only) { throw "Required local fixture must be local-only: $($fixture.id)." }
        if (-not $seenIds.Add([string]$fixture.id) -or -not $seenNames.Add([string]$fixture.filename)) { throw "Duplicate required local fixture id or filename: $($fixture.id)." }
        if ([IO.Path]::IsPathRooted($fixture.filename) -or [IO.Path]::GetFileName([string]$fixture.filename) -ne $fixture.filename -or $fixture.filename -in @('.', '..')) { throw "Required local fixture filename is unsafe: $($fixture.id)." }
        if ([string]$fixture.sha256 -notmatch '^[A-F0-9]{64}$' -or -not (Test-LocalContractInteger $fixture.byte_size 1 ([int64]::MaxValue)) -or -not (Test-LocalContractFiniteNumber $fixture.duration_seconds) -or [double]$fixture.duration_seconds -le 0) { throw "Required local fixture identity is invalid: $($fixture.id)." }
        foreach ($field in 'codec', 'profile', 'pixel_format', 'rate', 'width', 'height', 'has_b_frames', 'frame_count', 'keyframe_count', 'picture_type_counts') {
            if ($null -eq $fixture.video.$field -or [string]::IsNullOrWhiteSpace([string]$fixture.video.$field)) { throw "Required local video fixture is missing ${field}: $($fixture.id)." }
        }
        if ([string]$fixture.video.rate -notmatch '^([1-9]\d{0,17})/([1-9]\d{0,17})$' -or -not (Test-LocalContractInteger $fixture.video.width 1 16384) -or -not (Test-LocalContractInteger $fixture.video.height 1 16384) -or -not (Test-LocalContractInteger $fixture.video.has_b_frames 0 16) -or -not (Test-LocalContractInteger $fixture.video.frame_count 1 1000000) -or -not (Test-LocalContractInteger $fixture.video.keyframe_count 1 1000000)) { throw "Required local video metadata is invalid: $($fixture.id)." }
        foreach ($pictureType in 'I', 'P', 'B') { if (-not (Test-LocalContractInteger $fixture.video.picture_type_counts.$pictureType 0 1000000)) { throw "Required local video picture-type count is invalid: $($fixture.id)." } }
        $pictureTypeTotal = [int64]$fixture.video.picture_type_counts.I + [int64]$fixture.video.picture_type_counts.P + [int64]$fixture.video.picture_type_counts.B
        if ($pictureTypeTotal -ne [int64]$fixture.video.frame_count -or [int64]$fixture.video.keyframe_count -gt [int64]$fixture.video.picture_type_counts.I -or (($fixture.video.picture_type_counts.B -gt 0) -ne ($fixture.video.has_b_frames -gt 0))) { throw "Required local video frame evidence is inconsistent: $($fixture.id)." }
        if (-not (Test-LocalContractInteger $fixture.encoder_intent.g 1 1000000) -or -not (Test-LocalContractInteger $fixture.encoder_intent.bf 0 16) -or -not (Test-LocalContractInteger $fixture.encoder_intent.idr_interval 0 1000000)) { throw "Required local encoder intent is invalid: $($fixture.id)." }
    }
}

Assert-LocalCorpusContractSchema $manifest.real_media_corpus
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
    if ($fixture.expected -eq 'success' -and -not ($fixture.PSObject.Properties.Name -contains 'video' -or $fixture.PSObject.Properties.Name -contains 'audio' -or $fixture.PSObject.Properties.Name -contains 'image')) { throw "Success fixture needs audio, video, or image metadata: $($fixture.id)." }
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
    if ($fixture.PSObject.Properties.Name -contains 'image') {
        foreach ($field in 'format', 'width', 'height', 'pixel_format') { if ($null -eq $fixture.image.$field -or [string]::IsNullOrWhiteSpace([string]$fixture.image.$field)) { throw "Image fixture is missing ${field}: $($fixture.id)." } }
        if ([int]$fixture.image.width -lt 1 -or [int]$fixture.image.height -lt 1) { throw "Image dimensions must be positive: $($fixture.id)." }
    }
}
if ($ManifestCoverageContractFixture) {
    # Test the real coverage assertion against an in-memory-only incomplete view.
    $manifest.fixtures = @($manifest.fixtures | Where-Object {
        -not ($_.PSObject.Properties.Name -contains 'audio') -or [int]$_.audio.channels -le 2
    })
}
if ($Manifest4kCoverageContractFixture) {
    # Test the real coverage assertion against an in-memory-only corpus with no 4K-class video.
    $manifest.fixtures = @($manifest.fixtures | Where-Object {
        -not ($_.PSObject.Properties.Name -contains 'video') -or
        [int]$_.video.width -lt 3840 -or [int]$_.video.height -lt 2160
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

function Assert-Manifest4kVideoCoverage([object[]]$Fixtures) {
    $has4kVideo = @($Fixtures | Where-Object {
        $_.PSObject.Properties.Name -contains 'video' -and
        [int]$_.video.width -ge 3840 -and [int]$_.video.height -ge 2160
    }).Count -gt 0
    if (-not $has4kVideo) {
        throw 'Manifest video coverage requires at least one 4K-class fixture (minimum 3840x2160).'
    }
}

Assert-ManifestAudioCoverage @($manifest.fixtures)
Assert-Manifest4kVideoCoverage @($manifest.fixtures)
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
function Assert-LocalContractEqual([object]$Actual, [object]$Expected, [string]$FixtureId, [string]$Label) { if ([string]$Actual -ne [string]$Expected) { throw "Required local fixture $FixtureId validation failed: $Label." } }
function Get-Sha256Hex([string]$Text) {
    $sha256 = [Security.Cryptography.SHA256]::Create()
    try { return ([Convert]::ToHexString($sha256.ComputeHash([Text.Encoding]::UTF8.GetBytes($Text)))) }
    finally { $sha256.Dispose() }
}
function Convert-PtsToMicroseconds([int64]$Pts, [int64]$TimeBaseNumerator, [int64]$TimeBaseDenominator) {
    $microseconds = ([decimal]$Pts * [decimal]$TimeBaseNumerator * 1000000) / [decimal]$TimeBaseDenominator
    return [int64][Math]::Round($microseconds, 0, [MidpointRounding]::AwayFromZero)
}
function Get-PngCrc32([byte[]]$Bytes) {
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
function Read-PngBigEndianUInt32([byte[]]$Bytes, [int]$Offset) {
    return [uint64]$Bytes[$Offset] * 16777216 + [uint64]$Bytes[$Offset + 1] * 65536 + [uint64]$Bytes[$Offset + 2] * 256 + [uint64]$Bytes[$Offset + 3]
}
function Get-RgbaPngMetadata([string]$Path) {
    $bytes = [IO.File]::ReadAllBytes($Path)
    $signature = [byte[]](0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A)
    if ($bytes.Length -lt 33) {
        throw "Invalid PNG signature: $Path"
    }
    for ($index = 0; $index -lt $signature.Length; $index++) {
        if ($bytes[$index] -ne $signature[$index]) { throw "Invalid PNG signature: $Path" }
    }
    $offset = 8
    $header = $null
    $idat = [Collections.Generic.List[byte]]::new()
    $seenIend = $false
    while ($offset -lt $bytes.Length) {
        if ($bytes.Length - $offset -lt 12) { throw "Truncated PNG chunk: $Path" }
        $length = Read-PngBigEndianUInt32 $bytes $offset
        if ($length -gt [uint64]($bytes.Length - $offset - 12)) { throw "PNG chunk length exceeds file bounds: $Path" }
        $dataOffset = $offset + 8
        $dataLength = [int]$length
        [byte[]]$data = [byte[]]::new($dataLength)
        if ($dataLength -gt 0) { [Array]::Copy($bytes, $dataOffset, $data, 0, $dataLength) }
        [byte[]]$typeBytes = [byte[]]::new(4)
        [Array]::Copy($bytes, $offset + 4, $typeBytes, 0, 4)
        $chunk = [Collections.Generic.List[byte]]::new()
        $chunk.AddRange($typeBytes)
        $chunk.AddRange($data)
        $expectedCrc = Read-PngBigEndianUInt32 $bytes ($dataOffset + $dataLength)
        if ([uint64](Get-PngCrc32 $chunk.ToArray()) -ne $expectedCrc) { throw "PNG chunk CRC mismatch: $Path" }
        $type = [Text.Encoding]::ASCII.GetString($typeBytes)
        if ($null -eq $header) {
            if ($type -ne 'IHDR' -or $dataLength -ne 13) { throw "PNG is missing a valid IHDR chunk: $Path" }
            $header = $data
        } elseif ($type -eq 'IDAT') {
            if ($seenIend) { throw "PNG has IDAT after IEND: $Path" }
            $idat.AddRange($data)
        } elseif ($type -eq 'IEND') {
            if ($dataLength -ne 0 -or $seenIend) { throw "PNG has an invalid IEND chunk: $Path" }
            $seenIend = $true
        }
        $offset += 12 + $dataLength
        if ($seenIend -and $offset -ne $bytes.Length) { throw "PNG has trailing data after IEND: $Path" }
    }
    if ($null -eq $header -or $idat.Count -eq 0 -or -not $seenIend) { throw "PNG is missing required IHDR, IDAT, or IEND data: $Path" }
    $width = Read-PngBigEndianUInt32 $header 0
    $height = Read-PngBigEndianUInt32 $header 4
    if ($width -lt 1 -or $height -lt 1 -or $width -gt 16384 -or $height -gt 16384 -or $header[8] -ne 8 -or $header[9] -ne 6 -or $header[10] -ne 0 -or $header[11] -ne 0 -or $header[12] -ne 0) {
        throw "PNG is not a bounded non-interlaced 8-bit RGBA image: $Path"
    }
    $expectedRawLength = [int]($height * (1 + 4 * $width))
    $compressed = [IO.MemoryStream]::new($idat.ToArray())
    $zlib = [IO.Compression.ZLibStream]::new($compressed, [IO.Compression.CompressionMode]::Decompress)
    $raw = [Collections.Generic.List[byte]]::new()
    $buffer = [byte[]]::new(8192)
    try {
        while (($read = $zlib.Read($buffer, 0, $buffer.Length)) -gt 0) {
            if ($raw.Count + $read -gt $expectedRawLength) { throw "PNG decompressed data exceeds RGBA scanline bounds: $Path" }
            for ($index = 0; $index -lt $read; $index++) { $raw.Add($buffer[$index]) }
        }
    }
    finally {
        $zlib.Dispose()
        $compressed.Dispose()
    }
    if ($raw.Count -ne $expectedRawLength) { throw "PNG decompressed data length is invalid: $Path" }
    $hasTransparentAlpha = $false
    $hasTranslucentAlpha = $false
    $hasOpaqueAlpha = $false
    $stride = 1 + 4 * [int]$width
    for ($y = 0; $y -lt $height; $y++) {
        $rowStart = $y * $stride
        if ($raw[$rowStart] -ne 0) { throw "PNG scanline filter must be None: $Path" }
        for ($x = 0; $x -lt $width; $x++) {
            $alpha = $raw[$rowStart + 1 + 4 * $x + 3]
            if ($alpha -eq 0) { $hasTransparentAlpha = $true }
            elseif ($alpha -eq 255) { $hasOpaqueAlpha = $true }
            else { $hasTranslucentAlpha = $true }
        }
    }
    if (-not $hasTransparentAlpha -or -not $hasTranslucentAlpha -or -not $hasOpaqueAlpha) { throw "PNG alpha coverage must include transparent, translucent, and opaque pixels: $Path" }
    return [pscustomobject]@{ width = $width; height = $height; pixel_format = 'rgba' }
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
    if (-not ($fixture.PSObject.Properties.Name -contains 'image')) {
        $duration = [double]$probe.format.duration
        if ([Math]::Abs($duration - [double]$fixture.duration_seconds) -gt 0.01) { throw "Duration for $($fixture.id) expected $($fixture.duration_seconds), got $duration." }
    }
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
    if ($fixture.PSObject.Properties.Name -contains 'image') {
        $image = @($probe.streams | Where-Object codec_type -eq 'video')[0]
        if ($null -eq $image) { throw "Missing image stream: $($fixture.id)" }
        Assert-Equal $image.codec_name $fixture.image.format "Image format for $($fixture.id)"
        $png = Get-RgbaPngMetadata $path
        Assert-Equal $png.width $fixture.image.width "Image width for $($fixture.id)"
        Assert-Equal $png.height $fixture.image.height "Image height for $($fixture.id)"
        Assert-Equal $png.pixel_format $fixture.image.pixel_format "Image pixel format for $($fixture.id)"
    }
}

if ($IncludeRealCorpus) {
    $corpusRoot = $env:MAELSTROM_REAL_MEDIA_ROOT
    if ([string]::IsNullOrWhiteSpace($corpusRoot)) { throw 'Set MAELSTROM_REAL_MEDIA_ROOT to an explicit local corpus directory.' }
    try { $resolvedCorpus = (Resolve-Path -LiteralPath $corpusRoot).Path }
    catch { throw 'Real-media corpus root is unavailable.' }
    $resolvedPrefix = $resolvedCorpus.TrimEnd('\', '/') + [IO.Path]::DirectorySeparatorChar
    foreach ($fixture in @($manifest.real_media_corpus.required_local_fixtures)) {
        try { $matches = @(Get-ChildItem -LiteralPath $resolvedCorpus -File -Recurse | Where-Object { $_.Name -ceq $fixture.filename }) }
        catch { throw "Required local fixture discovery failed: $($fixture.id)." }
        if ($matches.Count -eq 0) { throw "Missing required local fixture: $($fixture.filename)." }
        if ($matches.Count -ne 1) { throw "Required local fixture filename is not unique: $($fixture.filename)." }
        try { $path = (Resolve-Path -LiteralPath $matches[0].FullName).Path }
        catch { throw "Required local fixture resolution failed: $($fixture.id)." }
        if (-not $path.StartsWith($resolvedPrefix, [StringComparison]::OrdinalIgnoreCase)) { throw "Required local fixture escaped corpus root: $($fixture.filename)." }
        Assert-LocalContractEqual (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash $fixture.sha256 $fixture.id 'hash mismatch'
        Assert-LocalContractEqual (Get-Item -LiteralPath $path).Length $fixture.byte_size $fixture.id 'byte-size mismatch'
        $json = & $ffprobe -v error -show_streams -show_format -of json $path 2>$null
        if ($LASTEXITCODE -ne 0) { throw "ffprobe failed for required local fixture: $($fixture.id)" }
        $probe = $json | ConvertFrom-Json
        Assert-LocalContractEqual $probe.format.format_name $fixture.container $fixture.id 'container mismatch'
        $duration = [double]$probe.format.duration
        if ([Math]::Abs($duration - [double]$fixture.duration_seconds) -gt 0.01) { throw "Required local fixture $($fixture.id) validation failed: duration mismatch." }
        $video = @($probe.streams | Where-Object codec_type -eq 'video')[0]
        if ($null -eq $video) { throw "Missing video stream for required local fixture: $($fixture.id)" }
        Assert-LocalContractEqual $video.codec_name $fixture.video.codec $fixture.id 'video codec mismatch'
        Assert-LocalContractEqual $video.profile $fixture.video.profile $fixture.id 'video profile mismatch'
        Assert-LocalContractEqual $video.pix_fmt $fixture.video.pixel_format $fixture.id 'video pixel-format mismatch'
        Assert-LocalContractEqual $video.r_frame_rate $fixture.video.rate $fixture.id 'video rate mismatch'
        Assert-LocalContractEqual $video.width $fixture.video.width $fixture.id 'video width mismatch'
        Assert-LocalContractEqual $video.height $fixture.video.height $fixture.id 'video height mismatch'
        Assert-LocalContractEqual $video.has_b_frames $fixture.video.has_b_frames $fixture.id 'video B-frame mismatch'
        Assert-LocalContractEqual $video.nb_frames $fixture.video.frame_count $fixture.id 'video frame-count mismatch'
        $frameJson = & $ffprobe -v error -select_streams v:0 -show_frames -show_entries frame=key_frame,pict_type -of json $path 2>$null
        if ($LASTEXITCODE -ne 0) { throw "ffprobe frame scan failed for required local fixture: $($fixture.id)" }
        $frames = @((($frameJson | ConvertFrom-Json).frames) | Where-Object { $_.pict_type -in @('I', 'P', 'B') })
        Assert-LocalContractEqual $frames.Count $fixture.video.frame_count $fixture.id 'scanned frame-count mismatch'
        Assert-LocalContractEqual @($frames | Where-Object { [int]$_.key_frame -eq 1 }).Count $fixture.video.keyframe_count $fixture.id 'keyframe-count mismatch'
        foreach ($pictureType in 'I', 'P', 'B') { Assert-LocalContractEqual @($frames | Where-Object pict_type -eq $pictureType).Count $fixture.video.picture_type_counts.$pictureType $fixture.id "${pictureType}-frame-count mismatch" }
    }
    Write-Output "Real-media corpus: PASS ($(@($manifest.real_media_corpus.required_local_fixtures).Count) required local fixtures)"
}
Write-Output "Media fixtures: PASS ($($manifest.fixtures.Count) fixtures)"
