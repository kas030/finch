<#
.SYNOPSIS
Seeds and verifies last-page GUI restoration for Finch's generated large PDFs.
.DESCRIPTION
Uses only artifacts/gui-large-fixtures and new isolated data directories. Each
fixture gets one seed launch and one launch without a PDF argument. The latter
must restore its last page, scale, and normalized anchor, render visible tiles,
capture a valid P6 PPM screenshot, and exit without an application error.
Run separately from startup benchmarks: screenshot readback and 50 ms process
sampling are acceptance instrumentation, not uncontaminated startup/idle data.
.EXAMPLE
.\scripts\gui-large-acceptance.ps1 -ExpectedExecutableSha256 <release SHA256>
#>
[CmdletBinding()]
param(
    [string]$Executable = 'target/release/finch.exe',
    [string]$Pdfium = 'runtime/pdfium/bin/pdfium.dll',
    [string]$FixtureDirectory = 'artifacts/gui-large-fixtures',
    [string]$OutputDirectory,
    [ValidatePattern('^$|^[a-fA-F0-9]{64}$')][string]$ExpectedExecutableSha256 = '',
    [ValidateRange(2000, 120000)][int]$SeedDurationMs = 6000,
    [ValidateRange(2000, 120000)][int]$RestoreDurationMs = 8000,
    [ValidateRange(5000, 120000)][int]$WatchdogMarginMs = 20000,
    [ValidateRange(20, 1000)][int]$SampleIntervalMs = 50,
    [double]$AnchorTolerance = 0.002,
    [switch]$Gpu
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$FinchRoot = [IO.Path]::GetFullPath((Split-Path -Parent $PSScriptRoot))
Push-Location $FinchRoot
try {
    if ($AnchorTolerance -le 0 -or $AnchorTolerance -gt 0.02) { throw 'AnchorTolerance must be in (0, 0.02].' }
    $Executable = (Resolve-Path -LiteralPath $Executable).Path
    $Pdfium = (Resolve-Path -LiteralPath $Pdfium).Path
    $FixtureDirectory = (Resolve-Path -LiteralPath $FixtureDirectory).Path
    if ($Executable -notmatch '[/\\]release[/\\]') { throw 'Acceptance requires the Release executable.' }
    if (-not $OutputDirectory) {
        $OutputDirectory = Join-Path $FinchRoot ('artifacts/gui-large-acceptance-' + [DateTime]::UtcNow.ToString('yyyyMMdd-HHmmss'))
    }
    $OutputDirectory = [IO.Path]::GetFullPath($OutputDirectory)
    if (-not $OutputDirectory.StartsWith($FinchRoot + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'Acceptance output must be inside the workspace.'
    }
    if (Test-Path -LiteralPath $OutputDirectory) { throw "Output already exists: $OutputDirectory" }
    New-Item -ItemType Directory -Path $OutputDirectory | Out-Null

    function Write-Json([string]$Path, $Value) {
        [IO.File]::WriteAllText($Path, ($Value | ConvertTo-Json -Depth 30), [Text.UTF8Encoding]::new($false))
    }
    function Require([bool]$Condition, [string]$Message) {
        if (-not $Condition) { throw $Message }
    }
    function Quote-Argument([string]$Value) {
        if ($Value.Contains('"')) { throw 'A process argument contains an unsupported quotation mark.' }
        return '"' + ($Value -replace '(\\+)$', '$1$1') + '"'
    }
    function Normalize-DocumentPath([string]$Path) {
        if ($Path.StartsWith('\\?\UNC\', [StringComparison]::OrdinalIgnoreCase)) { $Path = '\\' + $Path.Substring(8) }
        elseif ($Path.StartsWith('\\?\', [StringComparison]::OrdinalIgnoreCase)) { $Path = $Path.Substring(4) }
        return [IO.Path]::GetFullPath($Path)
    }
    function Check-Ppm([string]$Path) {
        Require (Test-Path -LiteralPath $Path) "Missing screenshot: $Path"
        $Stream = [IO.File]::OpenRead($Path)
        try {
            $Header = [Collections.Generic.List[byte]]::new()
            $Lines = 0
            while ($Lines -lt 3 -and $Header.Count -lt 128) {
                $Byte = $Stream.ReadByte()
                if ($Byte -lt 0) { break }
                $Header.Add([byte]$Byte)
                if ($Byte -eq 10) { $Lines++ }
            }
            $Text = [Text.Encoding]::ASCII.GetString($Header.ToArray())
            Require ($Text -match '^P6\n([0-9]+) ([0-9]+)\n255\n$') "Invalid Finch P6 PPM header: $Path"
            $Width = [int]$Matches[1]
            $Height = [int]$Matches[2]
            Require ($Width -gt 0 -and $Height -gt 0) "Empty screenshot: $Path"
            Require ($Stream.Length -eq $Header.Count + 3L * $Width * $Height) "Truncated screenshot pixels: $Path"
            return [pscustomobject]@{ path = $Path; width = $Width; height = $Height; bytes = $Stream.Length }
        } finally { $Stream.Dispose() }
    }
    function Run-Finch([string]$Label, [string]$DataDirectory, [string]$Pdf, [bool]$Seed, [int]$DurationMs) {
        $MetricsPath = Join-Path $OutputDirectory "$Label.metrics.json"
        $ScreenshotPath = Join-Path $OutputDirectory "$Label.ppm"
        $Arguments = @('--data-dir', $DataDirectory, '--pdfium', $Pdfium,
            '--metrics', $MetricsPath, '--screenshot', $ScreenshotPath, '--quit-after-ms', "$DurationMs")
        if ($Gpu) { $Arguments += '--gpu' } else { $Arguments += '--software' }
        if ($Seed) { $Arguments += @('--', $Pdf) }
        Write-Json (Join-Path $OutputDirectory "$Label.invocation.json") ([ordered]@{
            executable = $Executable; arguments = $Arguments; supplied_pdf_argument = $Seed
        })
        $Samples = [Collections.Generic.List[object]]::new()
        $Process = $null
        $ExitCode = $null
        $ProcessId = $null
        $Watch = [Diagnostics.Stopwatch]::StartNew()
        try {
            $Quoted = @($Arguments | ForEach-Object { Quote-Argument $_ })
            $Process = Start-Process -FilePath $Executable -ArgumentList $Quoted -WorkingDirectory $FinchRoot `
                -WindowStyle Hidden -PassThru `
                -RedirectStandardOutput (Join-Path $OutputDirectory "$Label.stdout.log") `
                -RedirectStandardError (Join-Path $OutputDirectory "$Label.stderr.log")
            $ProcessId = $Process.Id
            while (-not $Process.HasExited) {
                if ($Watch.ElapsedMilliseconds -gt $DurationMs + $WatchdogMarginMs) {
                    throw "Watchdog expired for $Label (PID $ProcessId)."
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
                Start-Sleep -Milliseconds $SampleIntervalMs
            }
            $Process.WaitForExit()
            $Process.Refresh()
            $ExitCode = $Process.ExitCode
            Require ($ExitCode -eq 0) "$Label exited with code $ExitCode."
        } finally {
            if ($null -ne $Process) {
                if (-not $Process.HasExited) { $Process.Kill(); $Process.WaitForExit() }
                $Process.Dispose()
            }
            $Watch.Stop()
            Write-Json (Join-Path $OutputDirectory "$Label.process.json") $Samples.ToArray()
        }
        Require (Test-Path -LiteralPath $MetricsPath) "Missing application metrics for $Label."
        $Metrics = Get-Content -LiteralPath $MetricsPath -Raw | ConvertFrom-Json
        Require ($null -eq $Metrics.error) "Application error in ${Label}: $($Metrics.error)"
        Require ($null -ne $Metrics.reading_state) "No reading state in $Label."
        Require ($Metrics.screenshot_captured -eq $true) "Screenshot did not complete in $Label."
        $ExpectedBackend = if ($Gpu) { 'wgpu' } else { 'tiny-skia' }
        Require ($Metrics.renderer_backend -eq $ExpectedBackend) "Unexpected renderer in $Label."
        Require ($Metrics.software_renderer_effective -eq (-not [bool]$Gpu)) "Renderer mode mismatch in $Label."
        foreach ($Stage in @('window_opened_ms', 'document_opened_ms', 'visible_tiles_ready_ms', 'capture_complete_ms')) {
            Require ($null -ne $Metrics.stages_ms.PSObject.Properties[$Stage]) "Missing $Stage in $Label."
        }
        Require ($Metrics.pdf_worker.started_renders -gt 0) "No PDF tiles were rendered in $Label."
        Require ($Samples.Count -ge 2) "Insufficient independent process samples in $Label."
        $Screenshot = Check-Ppm $ScreenshotPath
        $Late = @($Samples | Where-Object wall_ms -GE ($DurationMs - 1000))
        if ($Late.Count -eq 0) { $Late = @($Samples[$Samples.Count - 1]) }
        $Result = [pscustomobject]@{
            label = $Label; process_id = $ProcessId; exit_code = $ExitCode
            supplied_pdf_argument = $Seed; external_process_lifetime_ms = $Watch.Elapsed.TotalMilliseconds
            renderer_backend = $Metrics.renderer_backend; stages_ms = $Metrics.stages_ms
            observed_peak_private_bytes = ($Samples | Measure-Object private_bytes -Maximum).Maximum
            observed_peak_working_set_bytes = ($Samples | Measure-Object working_set_bytes -Maximum).Maximum
            late_private_bytes = ($Late | Measure-Object private_bytes -Average).Average
            late_working_set_bytes = ($Late | Measure-Object working_set_bytes -Average).Average
            sample_count = $Samples.Count; sample_interval_ms = $SampleIntervalMs
            tile_cache_bytes = $Metrics.tile_cache_bytes; pdf_worker = $Metrics.pdf_worker
            reading_state = $Metrics.reading_state; screenshot = $Screenshot; error = $Metrics.error
        }
        Write-Json (Join-Path $OutputDirectory "$Label.summary.json") $Result
        return $Result
    }

    $ExecutableHash = (Get-FileHash -LiteralPath $Executable -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($ExpectedExecutableSha256) {
        Require ($ExecutableHash -eq $ExpectedExecutableSha256.ToLowerInvariant()) 'Executable SHA256 differs from the requested final build.'
    }
    $ManifestPath = Join-Path $FixtureDirectory 'manifest.json'
    $Manifest = @(Get-Content -LiteralPath $ManifestPath -Raw | ConvertFrom-Json)
    $Definitions = @(
        [pscustomobject]@{ name = '1000-pages-vector.pdf'; label = 'vector-1000'; pages = 1000; last_page = 999; minimum_bytes = 100000L },
        [pscustomobject]@{ name = '511mib-scan.pdf'; label = 'scan-511mib'; pages = 31; last_page = 30; minimum_bytes = 500L * 1024 * 1024 }
    )
    $Results = [Collections.Generic.List[object]]::new()
    $Failure = $null
    try {
        foreach ($Fixture in $Definitions) {
            $Pdf = (Resolve-Path -LiteralPath (Join-Path $FixtureDirectory $Fixture.name)).Path
            $File = Get-Item -LiteralPath $Pdf
            $Expected = @($Manifest | Where-Object { [IO.Path]::GetFileName($_.path) -eq $Fixture.name })
            Require ($Expected.Count -eq 1) "Missing or ambiguous fixture manifest entry for $($Fixture.name)."
            Require ($File.Length -ge $Fixture.minimum_bytes -and $File.Length -eq $Expected[0].bytes) "Unexpected fixture byte size: $Pdf"
            Require ($Expected[0].pages -eq $Fixture.pages) "Unexpected fixture page count in manifest: $Pdf"
            $PdfHash = (Get-FileHash -LiteralPath $Pdf -Algorithm SHA256).Hash.ToLowerInvariant()
            Require ($PdfHash -eq $Expected[0].sha256.ToLowerInvariant()) "Fixture hash mismatch: $Pdf"
            $HeaderStream = [IO.File]::OpenRead($Pdf)
            try {
                $Header = [byte[]]::new(8)
                Require ($HeaderStream.Read($Header, 0, 8) -eq 8) "Truncated PDF: $Pdf"
                Require ([Text.Encoding]::ASCII.GetString($Header).StartsWith('%PDF-')) "Invalid PDF signature: $Pdf"
            } finally { $HeaderStream.Dispose() }
            $DataDirectory = Join-Path $OutputDirectory ($Fixture.label + '-data')
            New-Item -ItemType Directory -Path $DataDirectory | Out-Null
            Write-Output "Seeding $($Fixture.name) in its isolated data directory."
            $Seed = Run-Finch ($Fixture.label + '-seed') $DataDirectory $Pdf $true $SeedDurationMs
            Require ((Normalize-DocumentPath $Seed.reading_state.path).Equals($Pdf, [StringComparison]::OrdinalIgnoreCase)) 'Seed opened a different PDF.'
            Require ($Seed.reading_state.revision.len -eq $File.Length) 'Seed revision does not describe the real fixture.'

            $SessionPath = Join-Path $DataDirectory 'session.json'
            $Session = Get-Content -LiteralPath $SessionPath -Raw | ConvertFrom-Json
            Require ($Session.format_version -eq 1) 'Unsupported test session schema.'
            Require ($Session.session.state.document_id -eq $Seed.reading_state.document_id) 'Seed session/document ID mismatch.'
            Copy-Item -LiteralPath $SessionPath -Destination (Join-Path $OutputDirectory ($Fixture.label + '-seed.session.json'))
            $Session.session.version = [long]$Session.session.version + 1
            $Session.session.state.page = $Fixture.last_page
            $Session.session.state.zoom = [pscustomobject]@{ Scale = 2.0 }
            $Session.session.state.anchor = [pscustomobject]@{ page_x = 0.5; page_y = 0.35; viewport_x = 0.5; viewport_y = 0.25 }
            $Session.session.clean_shutdown = $true
            # Only this newly-created isolated snapshot is changed. The source
            # PDFs and per-document SQL state remain untouched by the harness.
            $TemporarySession = $SessionPath + '.acceptance.tmp'
            Write-Json $TemporarySession $Session
            [IO.File]::Replace($TemporarySession, $SessionPath, $SessionPath + '.acceptance.previous')
            Write-Json (Join-Path $OutputDirectory ($Fixture.label + '-requested.session.json')) $Session
            $ExpectedState = $Session.session.state
            Write-Output "Restoring $($Fixture.name), zero-based page $($Fixture.last_page), without a PDF argument."
            $Restored = Run-Finch ($Fixture.label + '-restore') $DataDirectory $Pdf $false $RestoreDurationMs
            $Actual = $Restored.reading_state
            Require ($Actual.document_id -eq $ExpectedState.document_id) 'Restored a different document ID.'
            Require ((Normalize-DocumentPath $Actual.path).Equals($Pdf, [StringComparison]::OrdinalIgnoreCase)) 'Restored a different source PDF.'
            Require ($Actual.revision.len -eq $File.Length -and $Actual.revision.modified_ms -eq $ExpectedState.revision.modified_ms) 'Restored source revision mismatch.'
            Require ($Actual.page -eq $Fixture.last_page) "Last-page restore failed for $($Fixture.name): expected $($Fixture.last_page), got $($Actual.page)."
            Require ($null -ne $Actual.zoom.PSObject.Properties['Scale']) 'Restored zoom is not a fixed scale.'
            Require ([Math]::Abs([double]$Actual.zoom.Scale - 2.0) -lt 0.00001) 'Restored zoom differs from 2.0.'
            $AnchorDifferences = [ordered]@{}
            foreach ($Coordinate in @('page_x', 'page_y', 'viewport_x', 'viewport_y')) {
                $Difference = [Math]::Abs([double]$Actual.anchor.$Coordinate - [double]$ExpectedState.anchor.$Coordinate)
                $AnchorDifferences[$Coordinate] = $Difference
                Require ($Difference -le $AnchorTolerance) "Anchor $Coordinate drifted by $Difference in $($Fixture.name)."
            }
            $SavedAgain = Get-Content -LiteralPath $SessionPath -Raw | ConvertFrom-Json
            Require ($SavedAgain.session.version -gt $Session.session.version) 'Exit did not advance the snapshot revision.'
            Require ($SavedAgain.session.clean_shutdown -eq $true) 'Final snapshot is not a controlled exit.'
            Require ($SavedAgain.session.state.page -eq $Fixture.last_page) 'Final snapshot lost the last-page position.'
            $Results.Add([pscustomobject]@{
                fixture = $Fixture.name; pdf = $Pdf; bytes = $File.Length; sha256 = $PdfHash
                manifest_pages = $Fixture.pages; expected_zero_based_page = $Fixture.last_page
                expected_state = $ExpectedState; anchor_absolute_differences = $AnchorDifferences
                seed = $Seed; restored = $Restored; passed = $true
            })
            Write-Output "Passed $($Fixture.name): page, zoom, anchor, source identity, screenshot, and clean exit."
        }
    } catch {
        $Failure = $_.Exception.Message
    } finally {
        $Report = [ordered]@{
            schema_version = 1; completed_at_utc = [DateTime]::UtcNow.ToString('o')
            passed = ($null -eq $Failure -and $Results.Count -eq 2); error = $Failure
            executable = $Executable; executable_sha256 = $ExecutableHash
            pdfium = $Pdfium; pdfium_sha256 = (Get-FileHash -LiteralPath $Pdfium -Algorithm SHA256).Hash.ToLowerInvariant()
            fixture_manifest = $ManifestPath; renderer_requested = $(if ($Gpu) { 'wgpu' } else { 'tiny-skia' })
            anchor_tolerance_normalized = $AnchorTolerance; sample_interval_ms = $SampleIntervalMs
            fixtures = $Results.ToArray()
            limitations = @(
                'This is GUI restoration acceptance, not a 30-run performance or idle CPU benchmark.'
                'Fixture bytes/hashes are checked against the generated manifest; the 511 MiB fixture contains actual RGB image streams.'
                'Application render milestones and PPM capture prove a rendered window was captured; screenshots still need visual review.'
                'Memory samples cover this Finch PID only; brief peaks between samples and GPU allocations are not measured.'
                'The declared page count comes from the fixture manifest; the GUI must independently reach its declared last page.'
            )
        }
        Write-Json (Join-Path $OutputDirectory 'summary.json') $Report
    }
    if ($Failure) { throw "$Failure Evidence: $OutputDirectory" }
    Write-Output (Join-Path $OutputDirectory 'summary.json')
} finally { Pop-Location }
