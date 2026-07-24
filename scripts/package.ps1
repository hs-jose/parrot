#!/usr/bin/env pwsh
# Builds release binaries and stages a distributable archive under dist/.
# Usage:
#   scripts/package.ps1 [-Target <triple>] [-Archive <zip|tar.gz>]
# Examples:
#   .\scripts\package.ps1                                    # build for current host
#   .\scripts\package.ps1 -Target aarch64-pc-windows-msvc    # cross-compile ARM64 Windows
#   .\scripts\package.ps1 -Target x86_64-unknown-linux-gnu   # cross-compile via `cross` if installed
#
# When `-Target` is omitted the host triple (from `rustc -vV`) is used and no
# `--target` flag is passed to cargo, so cargo falls back to the default
# `target/release/` output directory. When `-Target` is given cargo builds
# into `target/<target>/release/` and this script resolves that path.
[CmdletBinding()]
param(
    [string]$Target,
    [ValidateSet("zip", "tar.gz")]
    [string]$Archive = "zip"
)

$ErrorActionPreference = "Stop"

$projectRoot = Split-Path -Parent $PSScriptRoot

if (-not $Target) {
    $Target = (rustc -vV | Select-String "^host:").ToString() -replace "^host:\s*", ""
}
$packageName = "parrot-$Target"
$distDir = Join-Path $projectRoot "dist"
$stageDir = Join-Path $distDir $packageName

Write-Host "Target: $Target"
Write-Host "Building release binaries..."
$cargoArgs = @("build", "--release", "--workspace", "--manifest-path", (Join-Path $projectRoot "Cargo.toml"))
if ($Target) { $cargoArgs += @("--target", $Target) }
& cargo @cargoArgs
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

$releaseDir = if ($Target) {
    Join-Path $projectRoot "target" $Target "release"
} else {
    Join-Path $projectRoot "target" "release"
}

$parrotBin = Join-Path $releaseDir "parrot.exe"
$parrotdBin = Join-Path $releaseDir "parrotd.exe"
if (-not (Test-Path $parrotBin)) { throw "parrot.exe not found at $parrotBin" }
if (-not (Test-Path $parrotdBin)) { throw "parrotd.exe not found at $parrotdBin" }

Write-Host "Staging distribution..."
if (Test-Path $stageDir) { Remove-Item -Recurse -Force $stageDir }
New-Item -ItemType Directory -Path (Join-Path $stageDir "bin") -Force | Out-Null
New-Item -ItemType Directory -Path (Join-Path $stageDir "config") -Force | Out-Null

Copy-Item $parrotBin (Join-Path $stageDir "bin")
Copy-Item $parrotdBin (Join-Path $stageDir "bin")
Copy-Item (Join-Path $projectRoot "README.md") $stageDir

$userDataDir = $env:LOCALAPPDATA
(Get-Content (Join-Path $projectRoot "scripts" "parrot.toml.template")) `
    -replace '\{USER_DATA_DIR\}', $userDataDir.Replace('\', '/') `
    | Set-Content (Join-Path $stageDir "config" "parrot.toml")

$archivePath = if ($Archive -eq "zip") {
    $p = Join-Path $distDir "$packageName.zip"
    if (Test-Path $p) { Remove-Item $p -Force }
    Push-Location $distDir
    Compress-Archive -Path $packageName -DestinationPath $p
    Pop-Location
    $p
} else {
    $p = Join-Path $distDir "$packageName.tar.gz"
    if (Test-Path $p) { Remove-Item $p -Force }
    $tar = Get-Command tar -ErrorAction SilentlyContinue
    if ($tar) {
        Push-Location $distDir
        & tar -czf $p $packageName
        Pop-Location
    } else {
        throw "tar not available; use -Archive zip"
    }
    $p
}

Write-Host "Done:"
Write-Host "  Target:  $Target"
Write-Host "  Stage:   $stageDir"
Write-Host "  Archive: $archivePath"