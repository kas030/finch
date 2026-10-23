[CmdletBinding()]
param(
    [ValidateSet('x86_64-pc-windows-msvc', 'x86_64-pc-windows-gnu')]
    [string]$Target = 'x86_64-pc-windows-msvc',
    [string]$Toolchain = '1.99.1',
    [switch]$SkipBuild,
    [switch]$WithoutExample,
    [string]$OutputDirectory
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$FinchRoot = [IO.Path]::GetFullPath((Split-Path -Parent $PSScriptRoot))
Push-Location $FinchRoot
try {
    if (-not $SkipBuild) {
        $Packages = @('-p', 'finch-app')
        if (-not $WithoutExample) { $Packages += @('-p', 'finch-text-stats') }
        & cargo "+$Toolchain" build --release --locked --target $Target @Packages
        if ($LASTEXITCODE -ne 0) { throw 'Release build failed' }
    }
    $MetadataText = & cargo "+$Toolchain" metadata --locked --format-version 1 --filter-platform $Target
    if ($LASTEXITCODE -ne 0) { throw 'Cannot read locked Cargo dependency metadata' }
    $Metadata = $MetadataText | ConvertFrom-Json
    $Version = ($Metadata.packages | Where-Object name -eq 'finch-app').version
    if (-not $Version) { throw 'Cannot determine Finch version' }
    $Build = Join-Path $Metadata.target_directory "$Target/release"
    if (-not (Test-Path -LiteralPath (Join-Path $Build 'finch.exe'))) {
        $HostInfo = & rustc "+$Toolchain" -vV
        if (($HostInfo -join "`n") -match "(?m)^host: $([regex]::Escape($Target))$") {
            $Build = Join-Path $Metadata.target_directory 'release'
        }
    }
    if (-not (Test-Path -LiteralPath (Join-Path $Build 'finch.exe'))) { throw "Missing release executable in $Build" }
    $Pdfium = Join-Path $FinchRoot 'runtime/pdfium'
    $StampPath = Join-Path $Pdfium '.finch-pdfium.json'
    if (-not (Test-Path -LiteralPath $StampPath)) {
        throw 'Run scripts/fetch-pdfium.ps1 first to verify the pinned PDFium runtime.'
    }
    $Stamp = Get-Content -LiteralPath $StampPath -Raw | ConvertFrom-Json
    if ($Stamp.release -ne '7881' -or $Stamp.platform -ne 'windows-x86_64' -or
        $Stamp.archive_sha256 -ne '73cc0de638ac2095e7445bf56a38200a5b7c7ca0e9f4ba144598f2457377ac08' -or
        (Get-FileHash -LiteralPath (Join-Path $Pdfium 'bin/pdfium.dll') -Algorithm SHA256).Hash.ToLowerInvariant() -ne $Stamp.library_sha256) {
        throw 'PDFium runtime provenance or library checksum does not match the pinned Windows package.'
    }
    if (-not $OutputDirectory) { $OutputDirectory = Join-Path $FinchRoot "dist/Finch-$Version-windows-x86_64" }
    $OutputDirectory = [IO.Path]::GetFullPath($OutputDirectory)
    if (-not $OutputDirectory.StartsWith($FinchRoot + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'Output directory must be inside the Finch workspace.'
    }
    $Zip = $OutputDirectory + '.zip'
    if ((Test-Path -LiteralPath $OutputDirectory) -or (Test-Path -LiteralPath $Zip)) {
        throw "Output already exists: $OutputDirectory. Choose a new -OutputDirectory."
    }
    New-Item -ItemType Directory -Path $OutputDirectory -Force | Out-Null
    Copy-Item -LiteralPath (Join-Path $Build 'finch.exe') -Destination $OutputDirectory
    Copy-Item -LiteralPath (Join-Path $Pdfium 'bin/pdfium.dll') -Destination $OutputDirectory
    $Licenses = Join-Path $OutputDirectory 'licenses'
    New-Item -ItemType Directory -Path $Licenses | Out-Null
    Get-ChildItem -LiteralPath $FinchRoot -File | Where-Object Name -Match '^LICENSE' |
        ForEach-Object { Copy-Item -LiteralPath $_.FullName -Destination $Licenses }
    $PdfiumLicenses = Join-Path $Licenses 'pdfium'
    New-Item -ItemType Directory -Path $PdfiumLicenses | Out-Null
    Copy-Item -LiteralPath (Join-Path $Pdfium 'LICENSE') -Destination $PdfiumLicenses
    Copy-Item -LiteralPath (Join-Path $Pdfium 'licenses') -Destination $PdfiumLicenses -Recurse
    Copy-Item -LiteralPath (Join-Path $Pdfium 'VERSION') -Destination $PdfiumLicenses
    Copy-Item -LiteralPath $StampPath -Destination (Join-Path $PdfiumLicenses 'provenance.json')
    $Notices = [Collections.Generic.List[string]]::new()
    $Notices.Add('Finch third-party dependency notices. This inventory follows Cargo.lock and includes transitive dependencies.')
    $Notices.Add('PDFium 7881: https://github.com/bblanchon/pdfium-binaries/releases/tag/chromium%2F7881')
    $SupplementRoot = Join-Path $FinchRoot 'licenses/third-party'
    $SupplementRecords = Get-Content -LiteralPath (Join-Path $SupplementRoot 'sources.json') -Raw | ConvertFrom-Json
    foreach ($Record in $SupplementRecords) {
        $Supplement = [IO.Path]::GetFullPath((Join-Path $SupplementRoot $Record.file))
        if (-not $Supplement.StartsWith($SupplementRoot + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase) -or
            (Get-FileHash -LiteralPath $Supplement -Algorithm SHA256).Hash.ToLowerInvariant() -ne $Record.sha256) {
            throw "Supplemental license failed provenance check: $($Record.file)"
        }
    }
    Copy-Item -LiteralPath (Join-Path $SupplementRoot 'sources.json') -Destination (Join-Path $Licenses 'supplement-sources.json')
    Copy-Item -LiteralPath (Join-Path $FinchRoot 'licenses/NOTICE.txt') -Destination (Join-Path $Licenses 'NOTICE.txt')
    $LicenseFiles = @{}
    function Find-LicenseFiles([string]$Source) {
        @(Get-ChildItem -LiteralPath $Source -File -Recurse | Where-Object {
            $Relative = $_.FullName.Substring($Source.Length + 1)
            ($_.Name -match '^(LICENSE|COPYING|NOTICE|COPYRIGHT|OFL)([._-]|$)' -and $_.Extension -notin @('.rs', '.c', '.h', '.cpp', '.py')) -or
                $Relative -match '(?i)(^|[/\\])licenses?[/\\]'
        })
    }
    foreach ($Package in $Metadata.packages) {
        $Source = Split-Path -Parent $Package.manifest_path
        $Files = @(Find-LicenseFiles $Source)
        if ($Package.license_file) {
            $Declared = [IO.Path]::GetFullPath((Join-Path $Source $Package.license_file))
            if ($Declared.StartsWith($Source + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase) -and (Test-Path -LiteralPath $Declared)) {
                $Files += Get-Item -LiteralPath $Declared
            }
        }
        $LicenseFiles[$Package.id] = @($Files | Sort-Object FullName -Unique)
    }
    foreach ($Package in ($Metadata.packages | Sort-Object name, version)) {
        $Notices.Add("`n$($Package.name) $($Package.version)`nLicense: $($Package.license)`nAuthors: $($Package.authors -join ', ')`nSource: $($Package.repository)")
        $Source = Split-Path -Parent $Package.manifest_path
        $Files = @($LicenseFiles[$Package.id])
        if ($Files.Count -eq 0 -and $Metadata.workspace_members -contains $Package.id) {
            $Source = $FinchRoot
            $Files = @(Get-ChildItem -LiteralPath $Source -File | Where-Object Name -Match '^LICENSE')
            $Notices.Add('License text: inherited Finch workspace license.')
        }
        if ($Files.Count -eq 0 -and $Package.repository -in @('https://github.com/iced-rs/iced', 'https://github.com/retep998/winapi-rs')) {
            $Shared = $Metadata.packages | Where-Object { $_.repository -eq $Package.repository -and @($LicenseFiles[$_.id]).Count -gt 0 } | Select-Object -First 1
            if ($Shared) {
                $Source = Split-Path -Parent $Shared.manifest_path
                $Files = @($LicenseFiles[$Shared.id])
                $Notices.Add("License text: shared upstream repository license, from $($Shared.name) $($Shared.version).")
            }
        }
        $Destination = Join-Path $Licenses "rust/$($Package.name)-$($Package.version)"
        foreach ($File in $Files) {
            $Relative = $File.FullName.Substring($Source.Length + 1)
            $TargetFile = Join-Path $Destination $Relative
            New-Item -ItemType Directory -Force -Path (Split-Path -Parent $TargetFile) | Out-Null
            Copy-Item -LiteralPath $File.FullName -Destination $TargetFile
        }
        $Supplement = Join-Path $SupplementRoot "$($Package.name)-$($Package.version)"
        if (Test-Path -LiteralPath $Supplement) {
            New-Item -ItemType Directory -Force -Path $Destination | Out-Null
            Get-ChildItem -LiteralPath $Supplement -File | ForEach-Object { Copy-Item -LiteralPath $_.FullName -Destination $Destination }
            $Notices.Add('License text: supplemental pinned source; see licenses/supplement-sources.json and licenses/NOTICE.txt.')
        } elseif ($Files.Count -eq 0) {
            throw "Missing license text for $($Package.name) $($Package.version); review it before packaging."
        }
    }
    $Notices | Set-Content -LiteralPath (Join-Path $OutputDirectory 'THIRD-PARTY-NOTICES.txt') -Encoding utf8
    if (-not $WithoutExample) {
        $Example = Join-Path $Build 'finch-text-stats.exe'
        if (-not (Test-Path -LiteralPath $Example)) { throw "Missing example plugin executable: $Example" }
        $Plugin = Join-Path $OutputDirectory 'plugins/finch.text-stats'
        New-Item -ItemType Directory -Force -Path $Plugin | Out-Null
        Copy-Item -LiteralPath $Example -Destination $Plugin
        $Manifest = Get-Content -LiteralPath (Join-Path $FinchRoot 'examples/text-stats/plugin.json') -Raw | ConvertFrom-Json
        $Manifest.executables = @{ 'windows-x86_64' = 'finch-text-stats.exe' }
        [IO.File]::WriteAllText((Join-Path $Plugin 'plugin.json'), ($Manifest | ConvertTo-Json -Depth 8), [Text.UTF8Encoding]::new($false))
    }
    @'
Finch — local PDF reading and management

Run finch.exe. Keep pdfium.dll in the same directory.
Use the plugin panel to install the optional plugins/finch.text-stats package;
it remains disabled until you explicitly enable it.
This development package is unsigned and has no automatic updater.
'@ | Set-Content -LiteralPath (Join-Path $OutputDirectory 'README.txt') -Encoding utf8
    Compress-Archive -LiteralPath $OutputDirectory -DestinationPath $Zip -CompressionLevel Optimal
    $Hash = (Get-FileHash -LiteralPath $Zip -Algorithm SHA256).Hash.ToLowerInvariant()
    "$Hash  $([IO.Path]::GetFileName($Zip))" | Set-Content -LiteralPath ($Zip + '.sha256') -Encoding ascii
    Write-Output $Zip
} finally { Pop-Location }
