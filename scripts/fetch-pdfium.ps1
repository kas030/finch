[CmdletBinding()]
param(
    [ValidateSet('windows-x86_64', 'macos-aarch64', 'macos-x86_64')]
    [string]$Platform = 'windows-x86_64',
    [string]$Archive,
    [string]$Destination
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$FinchRoot = [IO.Path]::GetFullPath((Split-Path -Parent $PSScriptRoot))
$Release = '7881'
$Assets = @{
    'windows-x86_64' = @('pdfium-win-x64.tgz', '73cc0de638ac2095e7445bf56a38200a5b7c7ca0e9f4ba144598f2457377ac08', 'bin/pdfium.dll')
    'macos-aarch64' = @('pdfium-mac-arm64.tgz', '52e94ca5aa8847934330daf3f8150c190682c5ca93831468794f8b90d4392e40', 'lib/libpdfium.dylib')
    'macos-x86_64' = @('pdfium-mac-x64.tgz', '6dedf83990e0e3d6b7c93c9e7589c5a126b0ae14b7464d76120cff7a26afb18b', 'lib/libpdfium.dylib')
}
$Asset, $ExpectedHash, $LibraryRelative = $Assets[$Platform]
function Assert-InWorkspace([string]$Path) {
    $Resolved = [IO.Path]::GetFullPath($Path)
    if (-not $Resolved.StartsWith($FinchRoot + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Refusing filesystem operation outside the Finch workspace: $Resolved"
    }
    return $Resolved
}
if (-not $Destination) { $Destination = Join-Path $FinchRoot 'runtime/pdfium' }
$Destination = Assert-InWorkspace $Destination
if ($Destination -ne (Join-Path $FinchRoot 'runtime/pdfium') -and
    -not $Destination.StartsWith((Join-Path $FinchRoot 'target') + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
    throw 'Destination must be runtime/pdfium or a child of target/; source directories cannot be replaced.'
}
$Parent = Split-Path -Parent $Destination
New-Item -ItemType Directory -Force -Path $Parent | Out-Null

if (-not $Archive) {
    $Cache = Join-Path $FinchRoot "target/downloads/pdfium-$Release"
    New-Item -ItemType Directory -Force -Path $Cache | Out-Null
    $Archive = Join-Path $Cache $Asset
    if (-not (Test-Path -LiteralPath $Archive) -or (Get-FileHash -LiteralPath $Archive -Algorithm SHA256).Hash.ToLowerInvariant() -ne $ExpectedHash) {
        $Download = Assert-InWorkspace ($Archive + '.download')
        $Url = "https://github.com/bblanchon/pdfium-binaries/releases/download/chromium%2F$Release/$Asset"
        & curl.exe --fail --location --retry 3 --proto '=https' --tlsv1.2 --output $Download $Url
        if ($LASTEXITCODE -ne 0) { throw "PDFium download failed ($LASTEXITCODE)" }
        if ((Get-FileHash -LiteralPath $Download -Algorithm SHA256).Hash.ToLowerInvariant() -ne $ExpectedHash) {
            Remove-Item -LiteralPath $Download -Force
            throw 'PDFium SHA-256 verification failed; the archive was not extracted.'
        }
        Move-Item -LiteralPath $Download -Destination $Archive -Force
    }
}
$Archive = (Resolve-Path -LiteralPath $Archive).Path
if ((Get-FileHash -LiteralPath $Archive -Algorithm SHA256).Hash.ToLowerInvariant() -ne $ExpectedHash) {
    throw 'PDFium SHA-256 verification failed; the archive was not extracted.'
}
$Stage = Assert-InWorkspace (Join-Path $Parent ('.pdfium-stage-' + [Guid]::NewGuid().ToString('N')))
$Backup = Assert-InWorkspace (Join-Path $Parent ('.pdfium-backup-' + [Guid]::NewGuid().ToString('N')))
New-Item -ItemType Directory -Path $Stage | Out-Null
try {
    $Entries = & tar -tzf $Archive
    if ($LASTEXITCODE -ne 0) { throw 'Cannot list PDFium archive' }
    foreach ($Entry in $Entries) {
        if ($Entry -match '(^[/\\])|(^[A-Za-z]:)|(^|[/\\])\.\.([/\\]|$)') { throw "Unsafe archive path: $Entry" }
    }
    & tar -xzf $Archive -C $Stage
    if ($LASTEXITCODE -ne 0) { throw 'Cannot extract PDFium archive' }
    foreach ($Required in @($LibraryRelative, 'LICENSE', 'licenses', 'VERSION')) {
        if (-not (Test-Path -LiteralPath (Join-Path $Stage $Required))) { throw "PDFium archive lacks $Required" }
    }
    $LibraryHash = (Get-FileHash -LiteralPath (Join-Path $Stage $LibraryRelative) -Algorithm SHA256).Hash.ToLowerInvariant()
    @{ release = $Release; platform = $Platform; archive_sha256 = $ExpectedHash; library_sha256 = $LibraryHash } |
        ConvertTo-Json | Set-Content -LiteralPath (Join-Path $Stage '.finch-pdfium.json') -Encoding utf8
    if (Test-Path -LiteralPath $Destination) {
        # Both final absolute paths were validated against the workspace above.
        Move-Item -LiteralPath $Destination -Destination $Backup
    }
    try { Move-Item -LiteralPath $Stage -Destination $Destination }
    catch {
        if (Test-Path -LiteralPath $Backup) { Move-Item -LiteralPath $Backup -Destination $Destination }
        throw
    }
    if (Test-Path -LiteralPath $Backup) { Remove-Item -LiteralPath $Backup -Recurse -Force }
} finally {
    if (Test-Path -LiteralPath $Stage) { Remove-Item -LiteralPath $Stage -Recurse -Force }
}
Write-Output (Join-Path $Destination $LibraryRelative)
