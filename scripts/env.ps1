$ErrorActionPreference = 'Stop'
$FinchRoot = Split-Path -Parent $PSScriptRoot
if (Test-Path (Join-Path $FinchRoot '.tools\cargo\bin')) {
    $env:CARGO_HOME = Join-Path $FinchRoot '.tools\cargo'
    $env:RUSTUP_HOME = Join-Path $FinchRoot '.tools\rustup'
    $env:PATH = (Join-Path $env:CARGO_HOME 'bin') + ';' + $env:PATH
}
if (Test-Path (Join-Path $FinchRoot '.tools\w64devkit\bin')) {
    $env:PATH = (Join-Path $FinchRoot '.tools\w64devkit\bin') + ';' + $env:PATH
}
$env:FINCH_PDFIUM_PATH = Join-Path $FinchRoot 'runtime\pdfium\bin\pdfium.dll'
