#!/usr/bin/env bash
#
# Build the Lumen Windows portable ZIP by CROSS-COMPILING from macOS/Linux
# via cargo-xwin (MSVC CRT + Windows SDK headers/libs are fetched by xwin;
# WinFsp headers and import lib are vendored by the winfsp-sys crate).
#
# Requirements:
#   - rustup target add x86_64-pc-windows-msvc
#   - cargo install cargo-xwin
#   - LLVM tools on PATH (llvm-lib); on macOS: brew install llvm and add
#     $(brew --prefix llvm)/bin
#   - WinFsp is a RUNTIME dependency of the end user, never bundled.
#
# Usage:
#   ./scripts/build-windows-zip.sh            # bundle + zip
#   ./scripts/build-windows-zip.sh --no-zip   # bundle only
#
# The PowerShell variant (build-windows-zip.ps1) remains for native
# Windows hosts and produces the identical layout.

set -euo pipefail

cd "$(dirname "$0")/.."

VERSION="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)"
DIST=dist
APP_DIR="$DIST/Lumen"
TARGET=x86_64-pc-windows-msvc

SKIP_ZIP=false
for arg in "$@"; do
    case "$arg" in
        --no-zip) SKIP_ZIP=true ;;
        *) echo "unknown flag: $arg" >&2; exit 2 ;;
    esac
done

echo "Building Lumen $VERSION Windows bundle (cross via cargo-xwin)..."

cargo xwin build --release --target "$TARGET" -p lumen-winfsp-shim --bin lumen-mount-winfsp
cargo xwin build --release --target "$TARGET" -p lumen-mcp-server --bin lumen-mcp-server
cargo xwin build --release --target "$TARGET" -p lumen-api-server --bin lumen-api-server

mkdir -p "$APP_DIR"
OUT="target/$TARGET/release"

cp "$OUT/lumen-mount-winfsp.exe" "$OUT/lumen-mcp-server.exe" "$OUT/lumen-api-server.exe" "$APP_DIR/"

# Ship the WinFsp user-mode DLL next to the binaries: harmless when the
# driver package is installed system-wide, useful for diagnosing setups.
WINFSP_SYS="$(ls -d ~/.cargo/registry/src/*/winfsp-sys-0.12.1+winfsp-2.1 | head -1)"
if [ -f "$WINFSP_SYS/winfsp/bin/winfsp-x64.dll" ]; then
    cp "$WINFSP_SYS/winfsp/bin/winfsp-x64.dll" "$APP_DIR/"
fi

cat > "$APP_DIR/README.txt" <<EOF
Welcome to Lumen $VERSION

Before first launch:
1. Install WinFsp from https://winfsp.dev/ (reboot if prompted)
2. Create a config file at %APPDATA%\Lumen\lumen.cmd exporting:
   LUMEN_INDEX_PATH, LUMEN_STORE_ENDPOINT, LUMEN_STORE_BUCKET,
   LUMEN_MOUNT_POINT, LUMEN_PASSWORD
3. Run lumen-mount.exe - your drive appears at the configured mount point

Optional sidecars:
- lumen-mcp-server.exe  MCP tools for agents (stdio/TCP)
- lumen-api-server.exe  auth, sync and billing service

The WinFsp driver is a runtime dependency and is never bundled here.
EOF

if $SKIP_ZIP; then
    echo "Bundle complete: $APP_DIR"
    exit 0
fi

ZIP_PATH="$DIST/Lumen-$VERSION-win64.zip"
( cd "$APP_DIR" && zip -qr "../Lumen-$VERSION-win64.zip" . )
echo "ZIP complete: $ZIP_PATH"

