# PowerShell build script for Windows Lumen distribution.
# Produces a portable release ZIP. Cross-compiling lumen-winfsp-shim from
# macOS is NOT possible: bindgen needs the WinFsp + Windows SDK headers.
# So this script runs ON a Windows box (or windows-2025 GitHub Actions runner):
#   cargo build --release -p lumen-winfsp-shim --bin lumen-mount-winfsp
#   etc., then run this script to package.

param(
    [string]$OutputDir = "dist",
    [switch]$SkipZip
)

$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")

$Version = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"(.+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
Write-Host "Building Lumen $Version Windows bundle..."

cargo build --release -p lumen-winfsp-shim --bin lumen-mount-winfsp
cargo build --release -p lumen-mcp-server --bin lumen-mcp-server
cargo build --release -p lumen-api-server --bin lumen-api-server

if (-not (Test-Path $OutputDir)) { New-Item -ItemType Directory -Path $OutputDir | Out-Null }
$AppDir = Join-Path $OutputDir "Lumen"
if (Test-Path $AppDir) { Remove-Item -Recurse -Force $AppDir }
New-Item -ItemType Directory -Path $AppDir | Out-Null

Copy-Item target/release/lumen-mount-winfsp.exe $AppDir/
Copy-Item target/release/lumen-mcp-server.exe target/release/lumen-api-server.exe $AppDir/

@"
Welcome to Lumen $Version

Before first launch:
1. Install WinFsp from https://winfsp.dev/ (reboot if prompted)
2. Create a config file at %APPDATA%\Lumen\lumen.cmd exporting:
   LUMEN_INDEX_PATH, LUMEN_STORE_ENDPOINT, LUMEN_STORE_BUCKET,
   LUMEN_MOUNT_POINT, LUMEN_PASSWORD
3. Run lumen-mount.exe - your drive appears at the configured mount point

The WinFsp driver is a runtime dependency and is never bundled here.
"@ | Set-Content (Join-Path $AppDir "README.txt")

if ($SkipZip) { Write-Host "Bundle complete: $AppDir"; exit 0 }

$ZipPath = Join-Path $OutputDir "Lumen-$Version-win64.zip"
Compress-Archive -Path "$AppDir\*" -DestinationPath $ZipPath -Force
Write-Host "ZIP complete: $ZipPath"

