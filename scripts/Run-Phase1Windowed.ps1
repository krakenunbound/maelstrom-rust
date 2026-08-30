#requires -Version 7.0
<#
.SYNOPSIS
Prepares, and only with -Run launches, the opt-in Phase 1 windowed UI probe.

.DESCRIPTION
Without -Run this script verifies the packaged runtime and the four existing
fixtures, writes four immutable schema-1 configurations, and records their
identity.  It never opens the editor by default.  -Run opens the packaged
editor through Launch-Maelstrom-Editor.bat and therefore requires explicit
editor-launch authorization from the operator running the script.
#>
[CmdletBinding()]
param(
    [switch]$Run,
    [ValidateRange(30, 180)]
    [int]$TimeoutSeconds = 180
)

$ErrorActionPreference = 'Stop'

function Write-AtomicUtf8File {
    param([Parameter(Mandatory)][string]$Path, [Parameter(Mandatory)][string]$Contents)
    $temporary = Join-Path ([IO.Path]::GetDirectoryName($Path)) ('.{0}.{1}.tmp' -f [IO.Path]::GetFileName($Path), [Guid]::NewGuid().ToString('N'))
    try {
        [IO.File]::WriteAllText($temporary, $Contents, [Text.UTF8Encoding]::new($false))
        [IO.File]::Move($temporary, $Path)
    } finally { Remove-Item -LiteralPath $temporary -Force -ErrorAction SilentlyContinue }
}

function Restore-EnvironmentValue {
    param([Parameter(Mandatory)][string]$Name, $Value)
    if ($null -eq $Value) { Remove-Item "Env:$Name" -ErrorAction SilentlyContinue }
    else { Set-Item "Env:$Name" $Value }
}

function Test-FiniteNonnegativeNumber($Value) {
    if (-not ((Test-Integer $Value) -or $Value -is [single] -or $Value -is [double] -or $Value -is [decimal])) { return $false }
    try { $number = [double]$Value } catch { return $false }
    return -not [double]::IsNaN($number) -and -not [double]::IsInfinity($number) -and $number -ge 0
}

function Test-Integer($Value) {
    return $Value -is [byte] -or $Value -is [sbyte] -or $Value -is [int16] -or $Value -is [uint16] -or
        $Value -is [int32] -or $Value -is [uint32] -or $Value -is [int64] -or $Value -is [uint64]
}

function Get-ProcessIdentity {
    param([Parameter(Mandatory)][int]$Id)
    $item = Get-CimInstance Win32_Process -Filter "ProcessId=$Id" -ErrorAction SilentlyContinue
    if ($null -eq $item) { return $null }
    return [pscustomobject]@{ Id = [int]$item.ProcessId; ParentId = [int]$item.ParentProcessId; CreationDate = (Convert-CimCreationDateUtc $item.CreationDate).ToString('o'); ExecutablePath = [string]$item.ExecutablePath }
}

function Test-ProcessIdentity {
    param($Identity)
    if ($null -eq $Identity) { return $false }
    $current = Get-ProcessIdentity -Id $Identity.Id
    return $null -ne $current -and $current.CreationDate -eq $Identity.CreationDate -and
        [string]::Equals($current.ExecutablePath, $Identity.ExecutablePath, [StringComparison]::OrdinalIgnoreCase)
}

function Convert-CimCreationDateUtc {
    param([Parameter(Mandatory)][datetime]$Value)
    # Get-CimInstance already converts DMTF to DateTime. Parsing its display string as DMTF fails.
    return $Value.ToUniversalTime()
}

function Get-DirectChildIdentities {
    param([Parameter(Mandatory)]$Parent)
    if (-not (Test-ProcessIdentity $Parent)) { return @() }
    return @(Get-CimInstance Win32_Process -Filter "ParentProcessId=$($Parent.Id)" -ErrorAction SilentlyContinue | ForEach-Object {
        [pscustomobject]@{ Id = [int]$_.ProcessId; ParentId = [int]$_.ParentProcessId; CreationDate = (Convert-CimCreationDateUtc $_.CreationDate).ToString('o'); ExecutablePath = [string]$_.ExecutablePath }
    })
}

function Get-DescendantIdentities {
    param([Parameter(Mandatory)]$Root)
    $descendants = [Collections.Generic.List[object]]::new()
    $pending = [Collections.Generic.Queue[object]]::new()
    $pending.Enqueue($Root)
    while ($pending.Count -gt 0) {
        foreach ($child in Get-DirectChildIdentities $pending.Dequeue()) {
            $descendants.Add($child)
            $pending.Enqueue($child)
        }
    }
    return @($descendants)
}

function Stop-OwnedProcessTree {
    param($Root)
    if ($null -eq $Root -or -not (Test-ProcessIdentity $Root)) { return }
    foreach ($child in Get-DirectChildIdentities $Root) { Stop-OwnedProcessTree $child }
    if (Test-ProcessIdentity $Root) { Stop-Process -Id $Root.Id -Force -ErrorAction SilentlyContinue }
}

function Assert-Fixtures {
    param([Parameter(Mandatory)][string[]]$Paths, [Parameter(Mandatory)][string]$Ffprobe)
    $hashes = [Collections.Generic.List[object]]::new()
    $seen = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    foreach ($path in $Paths) {
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "Missing Phase 1 fixture: $path" }
        $fullPath = [IO.Path]::GetFullPath($path)
        $hash = (Get-FileHash -LiteralPath $fullPath -Algorithm SHA256).Hash.ToLowerInvariant()
        if (-not $seen.Add($hash)) { throw "Phase 1 fixtures must have four distinct SHA-256 values: $fullPath" }
        $stream = & $Ffprobe -v error -select_streams v:0 -show_entries stream=codec_name,width,height,r_frame_rate,avg_frame_rate -of default=noprint_wrappers=1 $fullPath
        if ($LASTEXITCODE -ne 0) { throw "Fixture stream probe failed: $fullPath" }
        $duration = & $Ffprobe -v error -show_entries format=duration -of default=noprint_wrappers=1 $fullPath
        if ($LASTEXITCODE -ne 0 -or $stream -notcontains 'width=1920' -or $stream -notcontains 'height=1080' -or
            (($stream -notcontains 'r_frame_rate=30/1') -and ($stream -notcontains 'avg_frame_rate=30/1'))) {
            throw "Fixture is not a 1920x1080 30fps video: $fullPath"
        }
        $durationValue = @($duration | Where-Object { $_ -like 'duration=*' } | Select-Object -First 1)
        $seconds = 0.0
        if ($durationValue.Count -ne 1 -or -not [double]::TryParse((($durationValue[0] -split '=', 2)[1]), [Globalization.NumberStyles]::Float, [Globalization.CultureInfo]::InvariantCulture, [ref]$seconds) -or -not (Test-FiniteNonnegativeNumber $seconds) -or $seconds -lt 5.0) {
            throw "Fixture duration is less than five seconds: $fullPath"
        }
        $hashes.Add([ordered]@{ path = $fullPath; sha256 = $hash; bytes = (Get-Item -LiteralPath $fullPath).Length; duration_seconds=$seconds; stream=$stream })
    }
    return @($hashes)
}

function Assert-Distribution {
    param($Distribution, [double[]]$Values, [string]$Name)
    if ($null -eq $Distribution -or $Distribution.samples -ne 40 -or $Values.Count -ne 40) { throw "$Name distribution must contain exactly 40 measured samples." }
    foreach ($property in @('p50_ms', 'p95_ms', 'max_ms')) {
        if (-not (Test-FiniteNonnegativeNumber $Distribution.$property)) { throw "$Name distribution has invalid $property." }
    }
    $sorted = @($Values | Sort-Object)
    $expected = @($sorted[19], $sorted[37], $sorted[39])
    $actual = @([double]$Distribution.p50_ms, [double]$Distribution.p95_ms, [double]$Distribution.max_ms)
    for ($index = 0; $index -lt 3; $index++) {
        if ([math]::Abs($actual[$index] - $expected[$index]) -gt 0.000001) { throw "$Name distribution does not use nearest-rank values from the measured samples." }
    }
}

function Assert-AppReport {
    param($Report, [string]$RunId, [string]$ConfigPath, [string[]]$Sources, [string]$Adapter, [int]$SourceCount, [int]$ExpectedProcessId)
    if ($Report.schema_version -ne 1 -or $Report.status -ne 'completed' -or $Report.run_id -ne $RunId -or $Report.process_id -ne $ExpectedProcessId -or
        $Report.warmup_samples -ne 8 -or $Report.measured_samples -ne 40 -or $Report.cpu_budgets_passed -isnot [bool] -or $Report.cpu_budgets_passed -ne $true -or $null -ne $Report.failure) { throw 'App report identity, completion, or sample count is invalid.' }
    if ($null -eq $Report.configuration -or $Report.configuration.schema_version -ne 1 -or $Report.configuration.run_id -ne $RunId -or
        $Report.configuration.report_path -ne (Join-Path ([IO.Path]::GetDirectoryName($ConfigPath)) 'app-report.json') -or $Report.configuration.adapter_class -ne $Adapter -or
        @($Report.configuration.source_paths).Count -ne $SourceCount) { throw 'App report configuration identity is invalid.' }
    for ($index = 0; $index -lt $SourceCount; $index++) {
        if (-not [string]::Equals([string]$Report.configuration.source_paths[$index], $Sources[$index], [StringComparison]::OrdinalIgnoreCase)) { throw 'App report source identity differs from the versioned config.' }
    }
    $environment = $Report.environment
    if ($environment.renderer_device_type -ne $Adapter -or $environment.renderer_backend -ne 'Dx12' -or $environment.requested_output_size[0] -ne 1920 -or $environment.requested_output_size[1] -ne 1080 -or
        $environment.surface_size[0] -ne 1920 -or $environment.surface_size[1] -ne 1080) { throw 'App report did not prove requested Dx12 1920x1080 surface output.' }
    if ([string]::IsNullOrWhiteSpace($environment.renderer_name) -or @($environment.decoder_backends | Where-Object { -not [string]::IsNullOrWhiteSpace($_) }).Count -eq 0) { throw 'Renderer or observed decoder provenance is missing.' }
    foreach ($property in @('cache_bytes','cache_peak_bytes','cache_cap_bytes','active_sessions','peak_sessions','session_cap')) {
        if (-not (Test-Integer $environment.$property) -or $environment.$property -lt 0) { throw "Invalid resource field: $property" }
    }
    if ($environment.cache_cap_bytes -lt 1 -or $environment.cache_peak_bytes -gt $environment.cache_cap_bytes -or $environment.cache_bytes -gt $environment.cache_peak_bytes -or
        $environment.session_cap -ne 8 -or $environment.peak_sessions -gt $environment.session_cap -or $environment.active_sessions -gt $environment.peak_sessions) { throw 'Cache/session bounds failed.' }
    if (-not (Test-Integer $environment.runtime_diagnostics.monitor_errors) -or $environment.runtime_diagnostics.monitor_errors -ne 0) { throw 'Current monitor decode errors were observed or omitted.' }
    $samples = @($Report.samples)
    if ($samples.Count -ne 48 -or @($samples | Where-Object { $_.warmup -eq $true }).Count -ne 8 -or @($samples | Where-Object { $_.warmup -eq $false }).Count -ne 40) { throw 'App report must contain eight warmups and forty measured samples.' }
    $input = [Collections.Generic.List[double]]::new(); $cpu = [Collections.Generic.List[double]]::new(); $ready = [Collections.Generic.List[double]]::new()
    $sampleIndex = 0
    $previousPaint = 0
    foreach ($sample in $samples) {
        if (-not (Test-Integer $sample.index) -or $sample.index -ne $sampleIndex -or $sample.warmup -isnot [bool] -or $sample.warmup -ne ($sampleIndex -lt 8) -or
            -not (Test-Integer $sample.sequence_generation) -or $sample.sequence_generation -ne $samples[0].sequence_generation) { throw 'Sample order, warmup, or fixed timeline generation is invalid.' }
        $sampleIndex++
        foreach ($property in @('input_to_ui_cpu_ms', 'full_cpu_frame_ms', 'input_to_surface_submission_ms', 'matching_layers_to_surface_ms')) {
            if (-not (Test-FiniteNonnegativeNumber $sample.$property)) { throw "Sample $($sample.index) has invalid $property." }
        }
        if (-not (Test-Integer $sample.expected_playhead_tick) -or -not (Test-Integer $sample.playhead_tick) -or $sample.expected_playhead_tick -ne $sample.playhead_tick -or
            -not (Test-Integer $sample.paint_serial_before_input) -or -not (Test-Integer $sample.paint_serial) -or $sample.paint_serial -le $sample.paint_serial_before_input -or $sample.paint_serial -le $previousPaint -or
            @($sample.targets).Count -ne $SourceCount -or @($sample.layers).Count -ne $SourceCount) { throw "Sample $($sample.index) lacks exact playhead, paint, or layer evidence." }
        $mediaIds = @($sample.targets | ForEach-Object { $_.media_id } | Sort-Object -Unique)
        $previousPaint = $sample.paint_serial
        $slots = @($sample.targets | ForEach-Object { $_.slot } | Sort-Object -Unique)
        if (($slots -join ',') -ne ((0..($SourceCount - 1)) -join ',')) { throw 'Target slots are not the exact contributing layer set.' }
        foreach ($identity in @($sample.targets) + @($sample.layers)) {
            foreach ($property in @('slot','media_id','clip_id','generation','request_id')) {
                if (-not (Test-Integer $identity.$property) -or $identity.$property -lt 0) { throw "Invalid integer layer identity: $property" }
            }
            if (@($identity.output_size).Count -ne 2 -or -not (Test-Integer $identity.output_size[0]) -or -not (Test-Integer $identity.output_size[1])) { throw 'Invalid layer raster dimensions.' }
        }
        if ($mediaIds.Count -ne $SourceCount -or (@($mediaIds | ForEach-Object { [int]$_ }) -join ',') -ne ((1..$SourceCount) -join ',')) { throw "Sample $($sample.index) does not identify the exact expected media set." }
        foreach ($target in @($sample.targets)) {
            if (-not (Test-Integer $target.slot) -or -not (Test-Integer $target.media_id) -or -not (Test-Integer $target.clip_id) -or $target.clip_id -lt 1 -or -not (Test-Integer $target.generation) -or -not (Test-Integer $target.request_id) -or $target.request_id -lt 1 -or -not (Test-Integer $target.requested_source_tick) -or
                $target.output_size[0] -ne 1920 -or $target.output_size[1] -ne 1080) { throw "Sample $($sample.index) has invalid target identity." }
            $layer = @($sample.layers | Where-Object { $_.slot -eq $target.slot -and $_.media_id -eq $target.media_id -and $_.clip_id -eq $target.clip_id -and $_.generation -eq $target.generation -and $_.request_id -eq $target.request_id -and $_.output_size[0] -eq 1920 -and $_.output_size[1] -eq 1080 })
            if ($layer.Count -ne 1 -or -not (Test-Integer $layer[0].source_tick) -or -not (Test-Integer $layer[0].upload_serial) -or -not (Test-FiniteNonnegativeNumber $layer[0].input_to_upload_ms) -or $layer[0].source_tick -lt $target.requested_source_tick -or ($layer[0].source_tick - $target.requested_source_tick) -gt 33334 -or $layer[0].upload_serial -lt 1) { throw "Sample $($sample.index) lacks exact native generation/request/media/output/tick/upload identity." }
        }
        if (-not $sample.warmup) { $input.Add([double]$sample.input_to_ui_cpu_ms); $cpu.Add([double]$sample.full_cpu_frame_ms); $ready.Add([double]$sample.matching_layers_to_surface_ms) }
    }
    Assert-Distribution $Report.input_to_ui_cpu @($input) 'input_to_ui_cpu'
    Assert-Distribution $Report.full_cpu_frame @($cpu) 'full_cpu_frame'
    Assert-Distribution $Report.matching_layers_to_surface @($ready) 'matching_layers_to_surface'
    if ($Report.input_to_ui_cpu.p95_ms -gt 1.0 -or $Report.full_cpu_frame.p95_ms -ge 8.0) { throw 'CPU latency budgets failed.' }
}

$repoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$launcher = Join-Path $repoRoot 'Launch-Maelstrom-Editor.bat'
$packagedExe = Join-Path $repoRoot 'dist\Maelstrom-Windows-x64\Maelstrom.exe'
$ffprobe = Join-Path $repoRoot 'dist\Maelstrom-Windows-x64\ffprobe.exe'
$fixtureRoot = Join-Path $repoRoot 'artifacts\phase1-multisource'
$fixtures = @(0, 60, 120, 180 | ForEach-Object { [IO.Path]::GetFullPath((Join-Path $fixtureRoot ('source-hue-{0:d3}.mp4' -f $_))) })
if (-not (Test-Path -LiteralPath $launcher -PathType Leaf) -or -not [string]::Equals([IO.Path]::GetFullPath($launcher), 'H:\Maelstrom Rust\Launch-Maelstrom-Editor.bat', [StringComparison]::OrdinalIgnoreCase)) { throw 'The only permitted launcher is H:\Maelstrom Rust\Launch-Maelstrom-Editor.bat.' }
if (-not (Test-Path -LiteralPath $packagedExe -PathType Leaf) -or -not (Test-Path -LiteralPath $ffprobe -PathType Leaf)) { throw 'Packaged Maelstrom.exe or approved packaged ffprobe.exe is missing.' }
if ($Run -and @(Get-CimInstance Win32_Process -Filter "Name='Maelstrom.exe'" -ErrorAction SilentlyContinue).Count -ne 0) { throw 'A Maelstrom editor is already running; windowed cases require an isolated session.' }

$savedEnvironment = @{}
$environmentNames = @('MAELSTROM_PHASE1_UI_CONFIG', 'MAELSTROM_PHASE0_SURFACE_ADAPTER_CLASS', 'MAELSTROM_SMOKE_EDITOR', 'MAELSTROM_STARTUP_REPORT', 'MAELSTROM_SURFACE_SUBMISSION_REPORT', 'MAELSTROM_MEDIA_ACCEPTANCE_PATH', 'MAELSTROM_MEDIA_ACCEPTANCE_REPORT', 'MAELSTROM_MEDIA_ACCEPTANCE_EXPORT_PATH', 'MAELSTROM_PLAYBACK_SOAK_REPORT', 'MAELSTROM_PLAYBACK_SOAK_SECONDS', 'MAELSTROM_PHASE0_REPORT', 'MAELSTROM_PHASE0_ARTIFACT_ROOT', 'MAELSTROM_PHASE0_MEDIA', 'MAELSTROM_PHASE0_CROSS_ADAPTER_GPU_REPORT', 'MAELSTROM_REQUIRE_GPU_TIMESTAMP_QUERY', 'MAELSTROM_PHASE1_MULTISOURCE_REPORT', 'MAELSTROM_PHASE1_LATENCY_REPORT', 'MAELSTROM_PHASE1_SUSTAINED_REPORT', 'MAELSTROM_PHASE1_SUSTAINED_SECONDS', 'MAELSTROM_PHASE1_GENERATION_STRESS_REPORT', 'MAELSTROM_PHASE1_LIVE_AUDIO_REPORT', 'MAELSTROM_PHASE1_LIVE_AUDIO_SECONDS', 'MAELSTROM_PHASE1_AUDIO_MEDIA')
$environmentNames += @('MAELSTROM_LAUNCHER_WAIT', 'MAELSTROM_DEMO_HUB')
foreach ($name in $environmentNames) { $savedEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, 'Process') }
$runDirectory = Join-Path $fixtureRoot ('windowed-' + [Guid]::NewGuid().ToString())
$wrapperPath = Join-Path $runDirectory 'windowed-wrapper.json'
$cases = [Collections.Generic.List[object]]::new()
$operationError = $null
$case = $null
$app = $null
$cmdIdentity = $null
$ownedDescendants = [Collections.Generic.List[object]]::new()

try {
    New-Item -ItemType Directory -Path $runDirectory -Force | Out-Null
    # Fail before the batch launcher's interactive missing-runtime pause.
    foreach ($runtime in @('avcodec-62.dll','avdevice-62.dll','avfilter-11.dll','avformat-62.dll','avutil-60.dll','swresample-6.dll','swscale-9.dll','libgcc_s_seh-1.dll','libstdc++-6.dll','libvpl.dll','libwinpthread-1.dll','vcruntime140.dll')) {
        if (-not (Test-Path -LiteralPath (Join-Path (Split-Path -Parent $packagedExe) $runtime) -PathType Leaf)) { throw "Missing packaged runtime: $runtime" }
    }
    & $launcher --verify-runtime
    if ($LASTEXITCODE -ne 0) { throw 'The exact packaged editor launcher did not verify its runtime.' }
    $fixtureHashes = Assert-Fixtures -Paths $fixtures -Ffprobe $ffprobe
    New-Item -ItemType Directory -Path $runDirectory -Force | Out-Null
    $exeHash = (Get-FileHash -LiteralPath $packagedExe -Algorithm SHA256).Hash.ToLowerInvariant()
    foreach ($adapter in @('IntegratedGpu', 'DiscreteGpu')) {
        foreach ($sourceCount in @(1, 4)) {
            $caseId = ('{0}-{1}-sources' -f $adapter.ToLowerInvariant(), $sourceCount)
            $caseDirectory = Join-Path $runDirectory $caseId
            New-Item -ItemType Directory -Path $caseDirectory -Force | Out-Null
            $selectedSources = @($fixtures | Select-Object -First $sourceCount)
            $runId = ('phase1-windowed-' + [Guid]::NewGuid().ToString('N'))
            $reportPath = Join-Path $caseDirectory 'app-report.json'
            $configPath = Join-Path $caseDirectory 'config.schema1.json'
            $configuration = [ordered]@{ schema_version = 1; run_id = $runId; source_paths = $selectedSources; report_path = $reportPath; adapter_class = $adapter }
            Write-AtomicUtf8File -Path $configPath -Contents (($configuration | ConvertTo-Json -Depth 4) + "`n")
            $case = [ordered]@{ case_id = $caseId; status = 'prepared'; adapter_class = $adapter; source_count = $sourceCount; run_id = $runId; configuration_path = $configPath; configuration_sha256 = (Get-FileHash -LiteralPath $configPath -Algorithm SHA256).Hash.ToLowerInvariant(); app_report_path = $reportPath; packaged_executable_path = $packagedExe; packaged_executable_sha256 = $exeHash; fixture_hashes = $fixtureHashes; renderer_requested_adapter = $adapter; renderer_requested_backend = 'Dx12'; physical_input_observed = $false; gpu_completion_observed = $false; physical_scanout_observed = $false; codec_or_backend_unknown_is_unproven = $true; failure = $null }
            $cases.Add($case)
            if ($Run) {
                foreach ($name in $environmentNames) { if ($name -notin @('MAELSTROM_PHASE1_UI_CONFIG', 'MAELSTROM_PHASE0_SURFACE_ADAPTER_CLASS')) { Remove-Item "Env:$name" -ErrorAction SilentlyContinue } }
                $env:MAELSTROM_PHASE1_UI_CONFIG = $configPath
                $env:MAELSTROM_PHASE0_SURFACE_ADAPTER_CLASS = $adapter
                $env:MAELSTROM_LAUNCHER_WAIT = '1'
                $launchTime = [DateTime]::UtcNow
                $cmd = Start-Process -FilePath $env:ComSpec -ArgumentList @('/d', '/c', ('call "{0}"' -f $launcher)) -WorkingDirectory $repoRoot -WindowStyle Hidden -PassThru
                $cmdIdentity = Get-ProcessIdentity -Id $cmd.Id
                if ($null -eq $cmdIdentity) { throw 'The waiting launcher exited before process ownership could be recorded.' }
                $case.launcher_cmd_process_id = $cmd.Id
                if ($null -ne $cmdIdentity) { $case.launcher_cmd_process_creation_date = $cmdIdentity.CreationDate }
                $app = $null; $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
                while ($null -eq $app -and [DateTime]::UtcNow -lt $deadline) {
                    $candidate = @(Get-CimInstance Win32_Process -Filter "ParentProcessId=$($cmd.Id)" -ErrorAction SilentlyContinue | Where-Object { [string]::Equals($_.ExecutablePath, $packagedExe, [StringComparison]::OrdinalIgnoreCase) -and (Convert-CimCreationDateUtc $_.CreationDate) -ge $launchTime } | Select-Object -First 1)
                    if ($candidate.Count -eq 1) { $app = Get-ProcessIdentity -Id ([int]$candidate[0].ProcessId) }
                    if ($null -eq $app -and -not (Test-ProcessIdentity $cmdIdentity)) { throw "$caseId launcher exited before an editor process could be observed." }
                    Start-Sleep -Milliseconds 100
                }
                if ($null -eq $app) { throw "$caseId did not launch a Maelstrom child through the exact batch launcher." }
                $case.app_process_id = $app.Id; $case.app_process_creation_date = $app.CreationDate
                $ownedDescendants = [Collections.Generic.List[object]]::new()
                while ((Test-ProcessIdentity $app) -and [DateTime]::UtcNow -lt $deadline) {
                    foreach ($descendant in Get-DescendantIdentities $app) { $ownedDescendants.Add($descendant) }
                    Start-Sleep -Milliseconds 100
                }
                if (Test-ProcessIdentity $app) { throw "$caseId exceeded its $TimeoutSeconds second bound." }
                if (-not $cmd.WaitForExit(5000)) { throw 'Waiting launcher did not exit after the app.' }
                if ($cmd.ExitCode -ne 0) { throw "Editor launcher returned exit code $($cmd.ExitCode)." }
                if (-not (Test-Path -LiteralPath $reportPath -PathType Leaf)) { throw "$caseId exited without an app report." }
                $appReport = Get-Content -LiteralPath $reportPath -Raw | ConvertFrom-Json
                $case.app_report_sha256 = (Get-FileHash -LiteralPath $reportPath -Algorithm SHA256).Hash.ToLowerInvariant(); $case.app_report = $appReport
                Assert-AppReport -Report $appReport -RunId $runId -ConfigPath $configPath -Sources $selectedSources -Adapter $adapter -SourceCount $sourceCount -ExpectedProcessId $app.Id
                if (@($ownedDescendants | Where-Object { Test-ProcessIdentity $_ }).Count -ne 0) { throw "$caseId left an owned child process after graceful app exit." }
                $case.status = 'passed'; $case.decoder_backends = @($appReport.environment.decoder_backends); $case.display_refresh_millihertz = $appReport.environment.display_refresh_millihertz; $case.gpu_stage_timings = $appReport.environment.gpu_stage_timings
                $cmd.Dispose()
            }
        }
    }
} catch {
    $operationError = $_
    if ($null -ne $case -and $case.status -eq 'prepared') {
        $case.status = 'failed'
        $case.failure = [ordered]@{ message = $_.Exception.Message }
    }
} finally {
    foreach ($entry in $savedEnvironment.GetEnumerator()) { Restore-EnvironmentValue -Name $entry.Key -Value $entry.Value }
    if ($null -ne $operationError -and $Run) {
        foreach ($descendant in $ownedDescendants) { Stop-OwnedProcessTree $descendant }
        Stop-OwnedProcessTree $app
        Stop-OwnedProcessTree $cmdIdentity
    }
    if (Test-Path -LiteralPath $runDirectory -PathType Container) {
        $status = if ($null -eq $operationError) { if ($Run) { 'passed' } else { 'prepared' } } else { 'failed' }
        $summary = [ordered]@{ schema_version = 1; status = $status; mode = if ($Run) { 'run' } else { 'prepare' }; run_directory = $runDirectory; packaged_executable_path = $packagedExe; cases = $cases; comparison = $null; failure = if ($null -eq $operationError) { $null } else { [ordered]@{ message = $operationError.Exception.Message } } }
        if ($Run -and $null -eq $operationError -and $cases.Count -eq 4) {
            $one = @($cases | Where-Object source_count -eq 1); $four = @($cases | Where-Object source_count -eq 4)
            $summary.comparison = [ordered]@{ note = 'Distribution deltas are recorded only; this runner defines no relative one-versus-four threshold.'; by_adapter = @($one | ForEach-Object { $single = $_; $multi = @($four | Where-Object adapter_class -eq $single.adapter_class)[0]; [ordered]@{ adapter_class = $single.adapter_class; input_to_ui_cpu_p95_delta_ms = [double]$multi.app_report.input_to_ui_cpu.p95_ms - [double]$single.app_report.input_to_ui_cpu.p95_ms; full_cpu_frame_p95_delta_ms = [double]$multi.app_report.full_cpu_frame.p95_ms - [double]$single.app_report.full_cpu_frame.p95_ms; matching_layers_to_surface_p95_delta_ms = [double]$multi.app_report.matching_layers_to_surface.p95_ms - [double]$single.app_report.matching_layers_to_surface.p95_ms } }) }
        }
        Write-AtomicUtf8File -Path $wrapperPath -Contents (($summary | ConvertTo-Json -Depth 14) + "`n")
    }
}

if ($null -ne $operationError) { throw $operationError }
Write-Host "Phase 1 windowed probe $(if ($Run) { 'passed' } else { 'prepared' }): $wrapperPath"
