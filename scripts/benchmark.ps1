[CmdletBinding()]
param(
    [string]$Executable = 'target/release/finch.exe',
    [string]$Pdf = 'tests/fixtures/finch-demo.pdf',
    [string]$Pdfium = 'runtime/pdfium/bin/pdfium.dll',
    [ValidateRange(1, 1000)][int]$WarmRuns = 30,
    [ValidateRange(0, 20)][int]$WarmupRuns = 2,
    [ValidateRange(1000, 60000)][int]$RunDurationMs = 2000,
    [ValidateRange(1000, 60000)][int]$IdleWarmupMs = 5000,
    [ValidateRange(1000, 60000)][int]$IdleSampleMs = 10000,
    [switch]$Software,
    [switch]$Gpu,
    [switch]$AllowDebug,
    [string]$OutputDirectory
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$FinchRoot = [IO.Path]::GetFullPath((Split-Path -Parent $PSScriptRoot))
Push-Location $FinchRoot
try {
    if ($Software -and $Gpu) { throw 'Choose either -Software or -Gpu, not both.' }
    $Executable = (Resolve-Path -LiteralPath $Executable).Path
    $Pdf = (Resolve-Path -LiteralPath $Pdf).Path
    $Pdfium = (Resolve-Path -LiteralPath $Pdfium).Path
    $IsReleasePath = $Executable -match '[/\\]release[/\\]'
    if (-not $IsReleasePath -and -not $AllowDebug) {
        throw 'Use a Release executable; -AllowDebug is only for script smoke checks, never performance acceptance.'
    }
    if (-not $OutputDirectory) {
        $OutputDirectory = Join-Path $FinchRoot ('artifacts/benchmarks/windows-' + [DateTime]::UtcNow.ToString('yyyyMMdd-HHmmss'))
    }
    $OutputDirectory = [IO.Path]::GetFullPath($OutputDirectory)
    if (-not $OutputDirectory.StartsWith($FinchRoot + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'Benchmark output must be inside the workspace.'
    }
    if (Test-Path -LiteralPath $OutputDirectory) { throw "Output already exists: $OutputDirectory" }
    New-Item -ItemType Directory -Path $OutputDirectory -Force | Out-Null
    $DataDirectory = Join-Path $OutputDirectory 'isolated-data'
    New-Item -ItemType Directory -Path $DataDirectory | Out-Null
    $Common = @('--data-dir', $DataDirectory, '--pdfium', $Pdfium)
    if ($Software) { $Common += '--software' }
    if ($Gpu) { $Common += '--gpu' }
    $SoftwareRequested = -not $Gpu
    $Runs = [Collections.Generic.List[object]]::new()

    function Write-Json([string]$Path, $Value) {
        [IO.File]::WriteAllText($Path, ($Value | ConvertTo-Json -Depth 20), [Text.UTF8Encoding]::new($false))
    }
    function Quote-Argument([string]$Value) {
        if ($Value.Contains('"')) { throw 'A process argument contains an unsupported quotation mark.' }
        # Start-Process joins ArgumentList; double trailing backslashes before the closing quote.
        return '"' + ($Value -replace '(\\+)$', '$1$1') + '"'
    }
    function Start-Finch([string]$Label, [string[]]$Arguments) {
        $Quoted = @($Arguments | ForEach-Object { Quote-Argument $_ })
        return Start-Process -FilePath $Executable -ArgumentList $Quoted -WorkingDirectory $FinchRoot `
            -WindowStyle Hidden -PassThru `
            -RedirectStandardOutput (Join-Path $OutputDirectory "$Label.stdout.log") `
            -RedirectStandardError (Join-Path $OutputDirectory "$Label.stderr.log")
    }
    function Wait-Finch($Process, [int]$TimeoutMs, $Watch) {
        $Samples = [Collections.Generic.List[object]]::new()
        while (-not $Process.HasExited) {
            if ($Watch.ElapsedMilliseconds -gt $TimeoutMs) {
                $Process.Kill()
                $Process.WaitForExit()
                throw "Finch process $($Process.Id) exceeded its watchdog timeout. Inspect stderr in $OutputDirectory."
            }
            $Process.Refresh()
            if (-not $Process.HasExited) {
                $Samples.Add([pscustomobject]@{
                    wall_ms = $Watch.Elapsed.TotalMilliseconds
                    private_bytes = $Process.PrivateMemorySize64
                    working_set_bytes = $Process.WorkingSet64
                    process_cpu_ms = $Process.TotalProcessorTime.TotalMilliseconds
                })
            }
            Start-Sleep -Milliseconds 50
        }
        $Process.WaitForExit()
        $Process.Refresh()
        if ($Process.ExitCode -ne 0) { throw "Finch exited with code $($Process.ExitCode). Inspect logs in $OutputDirectory." }
        return $Samples.ToArray()
    }
    function Run-Measured([string]$Label, [bool]$Seed) {
        $MetricsPath = Join-Path $OutputDirectory "$Label.metrics.json"
        $Arguments = $Common + @('--metrics', $MetricsPath, '--quit-after-ms', "$RunDurationMs")
        if ($Seed) { $Arguments += @('--', $Pdf) }
        $Watch = [Diagnostics.Stopwatch]::StartNew()
        $Process = Start-Finch $Label $Arguments
        try { $Samples = @(Wait-Finch $Process ($RunDurationMs + 15000) $Watch) }
        finally { $Process.Dispose() }
        $Watch.Stop()
        Write-Json (Join-Path $OutputDirectory "$Label.process.json") $Samples
        if (-not (Test-Path -LiteralPath $MetricsPath)) { throw "Missing metrics for $Label" }
        $Metrics = Get-Content -LiteralPath $MetricsPath -Raw | ConvertFrom-Json
        if ($Metrics.error) { throw "Application reported an error in ${Label}: $($Metrics.error)" }
        $ExpectedBackend = if ($SoftwareRequested) { 'tiny-skia' } else { 'wgpu' }
        if ($null -eq $Metrics.PSObject.Properties['software_renderer_effective'] -or
            $null -eq $Metrics.PSObject.Properties['renderer_backend'] -or
            $Metrics.software_renderer_effective -ne $SoftwareRequested -or
            $Metrics.renderer_backend -ne $ExpectedBackend) {
            throw "Unexpected or missing renderer selection in ${Label}; rebuild Finch and use matching benchmark flags."
        }
        foreach ($Required in @('window_opened_ms', 'document_opened_ms', 'visible_tiles_ready_ms')) {
            if ($null -eq $Metrics.stages_ms.PSObject.Properties[$Required]) { throw "Missing stage $Required in $Label" }
        }
        if ($null -eq $Metrics.reading_state) { throw "No restored document state in $Label" }
        if ($Samples.Count -lt 2) { throw "Insufficient process samples for $Label" }
        $Late = @($Samples | Where-Object wall_ms -GE ($RunDurationMs - 500))
        if ($Late.Count -eq 0) { $Late = @($Samples[-1]) }
        return [pscustomobject]@{
            label = $Label
            external_process_lifetime_ms = $Watch.Elapsed.TotalMilliseconds
            stages_ms = $Metrics.stages_ms
            renderer_backend = $Metrics.renderer_backend
            software_renderer_effective = $Metrics.software_renderer_effective
            observed_peak_private_bytes = ($Samples | Measure-Object private_bytes -Maximum).Maximum
            observed_peak_working_set_bytes = ($Samples | Measure-Object working_set_bytes -Maximum).Maximum
            late_private_bytes = ($Late | Measure-Object private_bytes -Average).Average
            late_working_set_bytes = ($Late | Measure-Object working_set_bytes -Average).Average
            tile_cache_bytes = $Metrics.tile_cache_bytes
            pdf_worker = $Metrics.pdf_worker
            reading_state = $Metrics.reading_state
            screenshot_captured = $Metrics.screenshot_captured
        }
    }

    Write-Output 'Preparing an isolated reading session (excluded from warm statistics; not a cold-boot sample).'
    $Seed = Run-Measured 'seed' $true
    Write-Json (Join-Path $OutputDirectory 'seed.summary.json') $Seed
    for ($Index = 1; $Index -le $WarmupRuns; $Index++) { $null = Run-Measured ('warmup-{0:D2}' -f $Index) $false }
    for ($Index = 1; $Index -le $WarmRuns; $Index++) {
        $Run = Run-Measured ('warm-{0:D2}' -f $Index) $false
        if ($Run.reading_state.document_id -ne $Seed.reading_state.document_id -or
            $Run.reading_state.page -ne $Seed.reading_state.page -or
            ($Run.reading_state.zoom | ConvertTo-Json -Compress) -ne ($Seed.reading_state.zoom | ConvertTo-Json -Compress)) {
            throw "Warm run $Index did not restore the seeded document/page/zoom mode."
        }
        $Runs.Add($Run)
        Write-Output ("Warm run {0}/{1}: window {2:N1} ms, visible tiles {3:N1} ms" -f $Index, $WarmRuns, $Run.stages_ms.window_opened_ms, $Run.stages_ms.visible_tiles_ready_ms)
    }

    # Separate idle run: no metrics, screenshot capture, smoke actions, or UI automation.
    $IdleDuration = $IdleWarmupMs + $IdleSampleMs + 1000
    $IdleWatch = [Diagnostics.Stopwatch]::StartNew()
    $IdleProcess = Start-Finch 'idle' ($Common + @('--quit-after-ms', "$IdleDuration"))
    try {
        while ($IdleWatch.ElapsedMilliseconds -lt $IdleWarmupMs) {
            if ($IdleProcess.HasExited) { throw 'Idle sample exited during warmup' }
            Start-Sleep -Milliseconds 50
        }
        $IdleProcess.Refresh()
        $CpuStart = $IdleProcess.TotalProcessorTime.TotalMilliseconds
        $SampleStart = $IdleWatch.Elapsed.TotalMilliseconds
        $IdleSamples = [Collections.Generic.List[object]]::new()
        while ($IdleWatch.Elapsed.TotalMilliseconds - $SampleStart -lt $IdleSampleMs) {
            if ($IdleProcess.HasExited) { throw 'Idle process exited before the sample completed' }
            $IdleProcess.Refresh()
            $IdleSamples.Add([pscustomobject]@{
                wall_ms = $IdleWatch.Elapsed.TotalMilliseconds
                private_bytes = $IdleProcess.PrivateMemorySize64
                working_set_bytes = $IdleProcess.WorkingSet64
            })
            Start-Sleep -Milliseconds 50
        }
        $IdleProcess.Refresh()
        $Elapsed = $IdleWatch.Elapsed.TotalMilliseconds - $SampleStart
        $CpuUsed = $IdleProcess.TotalProcessorTime.TotalMilliseconds - $CpuStart
        $Idle = [pscustomobject]@{
            sample_wall_ms = $Elapsed; process_cpu_ms = $CpuUsed
            cpu_percent_one_core = 100.0 * $CpuUsed / $Elapsed
            cpu_percent_machine = 100.0 * $CpuUsed / $Elapsed / [Environment]::ProcessorCount
            average_private_bytes = ($IdleSamples | Measure-Object private_bytes -Average).Average
            average_working_set_bytes = ($IdleSamples | Measure-Object working_set_bytes -Average).Average
            instrumentation = 'Process counters only; no metrics, screenshot, smoke, or UI automation flags.'
        }
        $null = Wait-Finch $IdleProcess ($IdleDuration + 15000) $IdleWatch
        Write-Json (Join-Path $OutputDirectory 'idle.process.json') $IdleSamples.ToArray()
    } finally {
        if (-not $IdleProcess.HasExited) { $IdleProcess.Kill(); $IdleProcess.WaitForExit() }
        $IdleProcess.Dispose()
    }
    function Distribution($Values) {
        $Sorted = @($Values | Sort-Object)
        if ($Sorted.Count -eq 0) { return $null }
        return [pscustomobject]@{
            count = $Sorted.Count; minimum = $Sorted[0]; median = $Sorted[[Math]::Ceiling($Sorted.Count * 0.5) - 1]
            p95 = $Sorted[[Math]::Ceiling($Sorted.Count * 0.95) - 1]; maximum = $Sorted[-1]
        }
    }
    $Stages = [ordered]@{}
    $StageNames = @($Runs | ForEach-Object { $_.stages_ms.PSObject.Properties.Name } | Sort-Object -Unique)
    foreach ($Name in $StageNames) {
        $Values = @($Runs | ForEach-Object { if ($null -ne $_.stages_ms.PSObject.Properties[$Name]) { $_.stages_ms.$Name } })
        $Stages[$Name] = Distribution $Values
    }
    $CpuMetadataSource = 'CIM'
    try { $CpuInfo = Get-CimInstance Win32_Processor | Select-Object -First 1 -ExpandProperty Name }
    catch {
        $CpuCimError = $_.Exception.Message
        try {
            $CpuInfo = Get-ItemPropertyValue -LiteralPath 'HKLM:\HARDWARE\DESCRIPTION\System\CentralProcessor\0' -Name ProcessorNameString -ErrorAction Stop
            $CpuMetadataSource = 'Registry'
        } catch {
            $CpuInfo = "Unavailable: CIM: $CpuCimError; registry: $($_.Exception.Message)"
            $CpuMetadataSource = 'Unavailable'
        }
    }
    $GpuMetadataSource = 'CIM'
    try { $GpuInfo = @(Get-CimInstance Win32_VideoController | Select-Object Name, DriverVersion) }
    catch {
        $GpuCimError = $_.Exception.Message
        try {
            $GpuInfo = @(Get-ChildItem -LiteralPath 'HKLM:\SYSTEM\CurrentControlSet\Control\Video' -ErrorAction Stop | ForEach-Object {
                $Adapter = Get-ItemProperty -LiteralPath (Join-Path $_.PSPath '0000') -ErrorAction SilentlyContinue
                if ($null -ne $Adapter -and $null -ne $Adapter.PSObject.Properties['DriverDesc'] -and $Adapter.DriverDesc) {
                    [pscustomobject]@{
                        Name = $Adapter.DriverDesc
                        DriverVersion = $(if ($null -ne $Adapter.PSObject.Properties['DriverVersion']) { $Adapter.DriverVersion } else { 'Unavailable' })
                    }
                }
            } | Sort-Object Name, DriverVersion -Unique)
            if ($GpuInfo.Count -eq 0) { throw 'No display adapter descriptions found.' }
            $GpuMetadataSource = 'Registry'
        } catch {
            $GpuInfo = @("Unavailable: CIM: $GpuCimError; registry: $($_.Exception.Message)")
            $GpuMetadataSource = 'Unavailable'
        }
    }
    $Summary = [ordered]@{
        schema_version = 1; measured_at_utc = [DateTime]::UtcNow.ToString('o')
        executable = $Executable; executable_sha256 = (Get-FileHash -LiteralPath $Executable).Hash.ToLowerInvariant()
        pdf = $Pdf; pdf_sha256 = (Get-FileHash -LiteralPath $Pdf).Hash.ToLowerInvariant()
        pdfium_sha256 = (Get-FileHash -LiteralPath $Pdfium).Hash.ToLowerInvariant()
        release_path = $IsReleasePath; software_renderer_requested = [bool]$SoftwareRequested
        renderer_selection = $(if ($Gpu) { 'explicit_gpu' } elseif ($Software) { 'explicit_software' } else { 'windows_default_software' })
        renderer_backend = $Seed.renderer_backend; software_renderer_effective = $Seed.software_renderer_effective
        machine = @{ os = [Environment]::OSVersion.VersionString; cpu = $CpuInfo; cpu_source = $CpuMetadataSource; logical_processors = [Environment]::ProcessorCount; gpu = $GpuInfo; gpu_source = $GpuMetadataSource }
        warm_runs = $WarmRuns; excluded_warmups = $WarmupRuns; run_duration_ms = $RunDurationMs
        stage_distributions_ms = $Stages
        late_private_bytes = Distribution @($Runs | ForEach-Object late_private_bytes)
        late_working_set_bytes = Distribution @($Runs | ForEach-Object late_working_set_bytes)
        observed_peak_private_bytes = Distribution @($Runs | ForEach-Object observed_peak_private_bytes)
        observed_peak_working_set_bytes = Distribution @($Runs | ForEach-Object observed_peak_working_set_bytes)
        idle = $Idle
        complete_30_run_release_sample = ($IsReleasePath -and $WarmRuns -ge 30)
        target_acceptance = 'Pending interpretation: internal milestones do not establish window interactivity or actual GPU presentation.'
        limitations = @(
            'Warm filesystem/driver cache runs; seed is not a controlled cold-start measurement.'
            'Stages begin at Rust main(), excluding loader time before main; external lifetime includes intentional quit delay.'
            'window_opened_ms is an event timestamp, not proof of interactivity.'
            'visible_tiles_ready_ms records prepared tiles, not independently observed presentation.'
            'Current --metrics instrumentation requests one screenshot readback after visible tiles; capture_complete_ms includes a deliberate 120 ms delay.'
            'Memory is independently sampled every 50 ms; sampled peaks can miss shorter peaks; GPU memory is not measured.'
            'Idle CPU is reported both as one-core utilization and machine-normalized utilization.'
            'A release-looking path alone is not build provenance; keep build command and executable hash with the report.'
        )
    }
    Write-Json (Join-Path $OutputDirectory 'runs.json') $Runs.ToArray()
    Write-Json (Join-Path $OutputDirectory 'summary.json') $Summary
    Write-Output (Join-Path $OutputDirectory 'summary.json')
} finally { Pop-Location }
