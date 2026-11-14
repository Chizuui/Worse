#!/usr/bin/env pwsh
[CmdletBinding()]
param([Parameter(ValueFromRemainingArguments = $true)][string[]]$TauriArgs)
Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent $PSScriptRoot
$App = Join-Path $Root "apps/harness-app"
if (-not (Get-Command pnpm -ErrorAction SilentlyContinue)) {
    throw "Required command 'pnpm' was not found on PATH."
}
if (-not (Test-Path (Join-Path $App "node_modules"))) {
    throw "Frontend dependencies are missing. Run: pnpm --dir apps/harness-app install"
}
Push-Location $App
try {
    & pnpm exec tauri build @TauriArgs
    if ($LASTEXITCODE -ne 0) {
        throw "tauri build failed with exit code $LASTEXITCODE."
    }
}
finally {
    Pop-Location
}
