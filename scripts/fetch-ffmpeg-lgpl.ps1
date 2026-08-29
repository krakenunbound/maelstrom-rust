[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$repoRoot = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$dependencyRoot = Join-Path $repoRoot '.deps\ffmpeg-lgpl-8.1'
$archive = Join-Path $repoRoot '.deps\ffmpeg-n8.1-win64-lgpl-shared.zip'
$url = 'https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-n8.1-latest-win64-lgpl-shared-8.1.zip'
$sha256 = '5a8278d43291930cc36e4da4486cf57f1ba7c1e88058addb4fc81fb32541e003'

New-Item -ItemType Directory -Path (Split-Path $archive) -Force | Out-Null
if (-not (Test-Path -LiteralPath $archive)) {
    Invoke-WebRequest -Uri $url -OutFile $archive -UseBasicParsing
}
$actual = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
if ($actual -ne $sha256) {
    throw "FFmpeg archive checksum mismatch. Expected $sha256, got $actual. The pinned bundle must be reviewed before updating."
}
if (-not (Test-Path -LiteralPath $dependencyRoot)) {
    Expand-Archive -LiteralPath $archive -DestinationPath $dependencyRoot
}
$bundle = Get-ChildItem -LiteralPath $dependencyRoot -Directory -Recurse |
    Where-Object { Test-Path -LiteralPath (Join-Path $_.FullName 'bin\ffmpeg.exe') } |
    Select-Object -First 1
if (-not $bundle) {
    throw 'The verified FFmpeg archive did not contain bin\ffmpeg.exe.'
}
$bundle.FullName
