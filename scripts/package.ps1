#!/usr/bin/env pwsh
$ErrorActionPreference = "Stop"

$projectRoot = Split-Path -Parent $PSScriptRoot
$target = "x86_64-pc-windows-msvc"
$packageName = "parrot-$target"
$distDir = Join-Path $projectRoot "dist"
$stageDir = Join-Path $distDir $packageName

Write-Host "Building release binaries..."
& cargo build --release --workspace --manifest-path (Join-Path $projectRoot "Cargo.toml")
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

Write-Host "Staging distribution..."
if (Test-Path $stageDir) { Remove-Item -Recurse -Force $stageDir }
New-Item -ItemType Directory -Path (Join-Path $stageDir "bin") -Force | Out-Null
New-Item -ItemType Directory -Path (Join-Path $stageDir "config") -Force | Out-Null

Copy-Item (Join-Path $projectRoot "target" "release" "parrot.exe") (Join-Path $stageDir "bin")
Copy-Item (Join-Path $projectRoot "target" "release" "parrotd.exe") (Join-Path $stageDir "bin")
Copy-Item (Join-Path $projectRoot "README.md") $stageDir

$userDataDir = "$env:LOCALAPPDATA"
(Get-Content (Join-Path $projectRoot "scripts" "parrot.toml.template")) `
    -replace '\{USER_DATA_DIR\}', $userDataDir.Replace('\', '/') `
    | Set-Content (Join-Path $stageDir "config" "parrot.toml")

$zipPath = Join-Path $distDir "$packageName.zip"
if (Test-Path $zipPath) { Remove-Item $zipPath }
Compress-Archive -Path "$stageDir\*" -DestinationPath $zipPath

Write-Host "Done:"
Write-Host "  Stage:  $stageDir"
Write-Host "  Zip:    $zipPath"
