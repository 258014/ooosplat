# Dump Brush's full CLI help so the training-parameter set can be checked
# against the shipped binary instead of against documentation.
#
# The engine manifest locks Brush 0.3.0 and the pipeline may only use flags that
# this binary really exposes, so the dump is the audit evidence for every Brush
# argument OOOSplat passes.
$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
$exe = Join-Path $root 'engines/brush/brush_app.exe'
$output = Join-Path $root 'docs/brush_help.txt'

if (-not (Test-Path -LiteralPath $exe)) {
    Write-Error "Brush binary not found: $exe"
}

New-Item -ItemType Directory -Force -Path (Split-Path -Parent $output) | Out-Null
& $exe --help 2>&1 | Out-String | Set-Content -LiteralPath $output -Encoding utf8

Write-Host "Wrote $output"
