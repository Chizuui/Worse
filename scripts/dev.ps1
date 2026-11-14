#!/usr/bin/env pwsh
[CmdletBinding()]
param(
    [switch]$BuildOnly,
    [Parameter(ValueFromRemainingArguments = $true)][string[]]$TauriArgs
)
Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent $PSScriptRoot
$SidecarBuild = Join-Path $PSScriptRoot "build-host-sidecar.mjs"
if (-not (Get-Command pnpm -ErrorAction SilentlyContinue)) {
    throw "Required command 'pnpm' was not found on PATH."
}
if (-not (Get-Command node -ErrorAction SilentlyContinue)) {
    throw "Required command 'node' was not found on PATH."
}
& node $SidecarBuild
if ($LASTEXITCODE -ne 0) {
    throw "Host sidecar build failed with exit code $LASTEXITCODE."
}
if ($BuildOnly -or $env:HARNESS_BUILD_ONLY -eq "1") {
    exit 0
}
Push-Location $Root
try {
    & pnpm --filter @harness/app exec tauri dev @TauriArgs
    if ($LASTEXITCODE -ne 0) {
        throw "tauri dev failed with exit code $LASTEXITCODE."
    }
}
finally {
    Pop-Location
}