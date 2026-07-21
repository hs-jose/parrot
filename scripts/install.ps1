#!/usr/bin/env pwsh
$ErrorActionPreference = "Stop"

$installDir = "$env:USERPROFILE\.parrot"
$binDir = "$installDir\bin"
$configDir = "$installDir\config"

New-Item -ItemType Directory -Path $binDir -Force | Out-Null
New-Item -ItemType Directory -Path $configDir -Force | Out-Null

$sourceDir = "$PSScriptRoot\..\dist\parrot-x86_64-pc-windows-msvc"
if (-not (Test-Path $sourceDir)) {
    throw "Distribution not found at $sourceDir. Run scripts/package.ps1 first."
}

Copy-Item "$sourceDir\bin\parrot.exe" $binDir -Force
Copy-Item "$sourceDir\bin\parrotd.exe" $binDir -Force

if (-not (Test-Path "$configDir\parrot.toml")) {
    $projectRoot = Split-Path -Parent $PSScriptRoot
    $userDataDir = $env:LOCALAPPDATA
    (Get-Content "$projectRoot\scripts\parrot.toml.template") `
        -replace '\{USER_DATA_DIR\}', $userDataDir.Replace('\', '/') `
        | Set-Content "$configDir\parrot.toml"
    Write-Host "Created default config at $configDir\parrot.toml"
}

Write-Host "Installed parrot to $binDir"
Write-Host "Add the following directory to your PATH:"
Write-Host "  $binDir"
