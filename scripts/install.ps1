#!/usr/bin/env pwsh
# Installs parrot into ~/.parrot/bin from a staged distribution.
# Usage:
#   .\scripts\install.ps1 [-Target <triple>] [-InstallDir <path>]
# Examples:
#   .\scripts\install.ps1
#   .\scripts\install.ps1 -Target aarch64-pc-windows-msvc
#   .\scripts\install.ps1 -InstallDir C:\Tools\parrot
#
# Config (`parrot.toml`) is created from the template on first install only;
# re-running install.ps1 preserves the existing config (e.g. user's API key).
[CmdletBinding()]
param(
    [string]$Target,
    [string]$InstallDir
)

$ErrorActionPreference = "Stop"

if (-not $InstallDir) { $InstallDir = Join-Path $env:USERPROFILE ".parrot" }
$binDir = Join-Path $InstallDir "bin"
$configDir = Join-Path $InstallDir "config"

New-Item -ItemType Directory -Path $binDir -Force | Out-Null
New-Item -ItemType Directory -Path $configDir -Force | Out-Null

if (-not $Target) {
    $Target = (rustc -vV | Select-String "^host:").ToString() -replace "^host:\s*", ""
}
$sourceDir = Join-Path $PSScriptRoot ".." "dist" "parrot-$Target"
$sourceDir = [System.IO.Path]::GetFullPath($sourceDir)
if (-not (Test-Path $sourceDir)) {
    throw "Distribution not found at $sourceDir. Run scripts/package.ps1 -Target $Target first."
}

$parrotSrc = Join-Path $sourceDir "bin" "parrot.exe"
$parrotdSrc = Join-Path $sourceDir "bin" "parrotd.exe"
if (-not (Test-Path $parrotSrc)) { throw "parrot.exe not found at $parrotSrc" }
if (-not (Test-Path $parrotdSrc)) { throw "parrotd.exe not found at $parrotdSrc" }

Copy-Item $parrotSrc (Join-Path $binDir "parrot.exe") -Force
Copy-Item $parrotdSrc (Join-Path $binDir "parrotd.exe") -Force

if (-not (Test-Path (Join-Path $configDir "parrot.toml"))) {
    $projectRoot = Split-Path -Parent $PSScriptRoot
    $userDataDir = $env:LOCALAPPDATA
    (Get-Content (Join-Path $projectRoot "scripts" "parrot.toml.template")) `
        -replace '\{USER_DATA_DIR\}', $userDataDir.Replace('\', '/') `
        | Set-Content (Join-Path $configDir "parrot.toml")
    Write-Host "Created default config at $configDir\parrot.toml"
}

Write-Host "Installed parrot to $binDir"
Write-Host "Add the following directory to your PATH:"
Write-Host "  $binDir"