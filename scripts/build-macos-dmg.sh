#!/usr/bin/env bash
#
# Build the Lumen macOS distribution: .app bundle containing all three
# release binaries plus a Finder launcher, packed into a DMG.
#
# Requirements:
#   - Rust stable toolchain (cargo in PATH)
#   - macOS host for the .app/DMG layout
#   - macFUSE is a RUNTIME dependency installed by the end user
#     (brew install --cask macfuse) - never bundled.
#   - Optional signing: SIGN_IDENTITY='Developer ID Application: ...'
#     signs binaries and bundle; leaves unsigned when unset.
#
# Usage:
#   ./scripts/build-macos-dmg.sh            # bundle + DMG
#   ./scripts/build-macos-dmg.sh --no-dmg   # bundle only

set -euo pipefail

cd "$(dirname "$0")/.."

VERSION="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)"
APP_NAME=Lumen
BUNDLE_ID=app.lumen.drive
DIST=dist
APP_ROOT="$DIST/$APP_NAME.app"
CONTENTS="$APP_ROOT/Contents"
MACOS_DIR="$CONTENTS/MacOS"

SKIP_DMG=false
for arg in "$@"; do
    case "$arg" in
        --no-dmg) SKIP_DMG=true ;;
        *) echo "unknown flag: $arg" >&2; exit 2 ;;
    esac
done

echo "Building Lumen $VERSION macOS bundle..."

rm -rf "$APP_ROOT"
mkdir -p "$MACOS_DIR"

cargo build --release -p lumen-fuse-shim --bin lumen-mount
cargo build --release -p lumen-mcp-server --bin lumen-mcp-server
cargo build --release -p lumen-api-server --bin lumen-api-server

cp target/release/lumen-mount target/release/lumen-mcp-server target/release/lumen-api-server "$MACOS_DIR/"

# LUMEN_UNIVERSAL=1 lipo-joins a second arch slice into every binary so
# Intel Macs (macOS 12+) can run the same bundle as Apple Silicon.
if [ "${LUMEN_UNIVERSAL:-0}" = "1" ]; then
    rustup target add x86_64-apple-darwin >/dev/null
    cargo build --release --target x86_64-apple-darwin -p lumen-fuse-shim --bin lumen-mount
    cargo build --release --target x86_64-apple-darwin -p lumen-mcp-server --bin lumen-mcp-server
    cargo build --release --target x86_64-apple-darwin -p lumen-api-server --bin lumen-api-server
    for bin in lumen-mount lumen-mcp-server lumen-api-server; do
        lipo -create "target/release/$bin" "target/x86_64-apple-darwin/release/$bin" -output "$MACOS_DIR/$bin"
        file "$MACOS_DIR/$bin" | grep -q 'universal' || { echo "lipo failed for $bin" >&2; exit 1; }
    done
fi

# Finder launcher: first run prints setup instructions instead of failing
# silently; afterwards it execs lumen-mount as the app lifecycle owner.
{
    printf '#!/usr/bin/env bash\n'
    printf '# Launcher for the Lumen suite inside the .app bundle. Prints\n'
    printf '# setup instructions on first run, then execs lumen-mount.\n'
    printf 'set -euo pipefail\n'
    printf 'HERE="$(cd "$(dirname "$0")" && pwd)"\n'
    printf 'CONFIG_DIR="$HOME/Library/Application Support/Lumen"\n'
    printf 'mkdir -p "$CONFIG_DIR"\n'
    printf 'ENV_FILE="$CONFIG_DIR/lumen.env"\n'
    printf 'if [ ! -f "$ENV_FILE" ]; then\n'
    printf '  echo "Lumen: no configuration found at $ENV_FILE" >&2\n'
    printf '  echo "Create it exporting: LUMEN_INDEX_PATH LUMEN_STORE_ENDPOINT" >&2\n'
    printf '  echo "  LUMEN_STORE_BUCKET LUMEN_MOUNT_POINT LUMEN_PASSWORD" >&2\n'
    printf '  exit 1\n'
    printf 'fi\n'
    printf '. "$ENV_FILE"\n'
    printf 'exec "$HERE/lumen-mount" \\\n'
    printf '    "${LUMEN_MOUNT_POINT:-/Volumes/Lumen}" \\\n'
    printf '    --index-path "${LUMEN_INDEX_PATH:?unset in config}" \\\n'
    printf '    --store-endpoint "${LUMEN_STORE_ENDPOINT:?unset in config}" \\\n'
    printf '    --bucket "${LUMEN_STORE_BUCKET:?unset in config}" \\\n'
    printf '    --password "${LUMEN_PASSWORD:?unset in config}"\n'
} > "$MACOS_DIR/$APP_NAME"
chmod +x "$MACOS_DIR/$APP_NAME"

# Info.plist: LSUIElement hides the dock icon (headless daemon app);
# users see /Volumes/Lumen appear - that IS the UI, matching Helumi's UX.
{
    printf '<?xml version="1.0" encoding="UTF-8"?>\n'
    printf '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">\n'
    printf '<plist version="1.0">\n<dict>\n'
    printf '    <key>CFBundleName</key><string>%s</string>\n' "$APP_NAME"
    printf '    <key>CFBundleDisplayName</key><string>%s</string>\n' "$APP_NAME"
    printf '    <key>CFBundleIdentifier</key><string>%s</string>\n' "$BUNDLE_ID"
    printf '    <key>CFBundleVersion</key><string>%s</string>\n' "$VERSION"
    printf '    <key>CFBundleShortVersionString</key><string>%s</string>\n' "$VERSION"
    printf '    <key>CFBundlePackageType</key><string>APPL</string>\n'
    printf '    <key>CFBundleExecutable</key><string>%s</string>\n' "$APP_NAME"
    printf '    <key>LSUIElement</key><true/>\n'
    printf '</dict>\n</plist>\n'
} > "$CONTENTS/Info.plist"

if [ -n "${SIGN_IDENTITY:-}" ]; then
    echo "Signing binaries and bundle as: $SIGN_IDENTITY"
    codesign --force --options runtime --sign "$SIGN_IDENTITY" "$MACOS_DIR/lumen-mount" \
        "$MACOS_DIR/lumen-mcp-server" "$MACOS_DIR/lumen-api-server" "$MACOS_DIR/$APP_NAME"
    codesign --force --sign "$SIGN_IDENTITY" "$APP_ROOT"
fi

echo "Bundle complete: $APP_ROOT"

if [ "$SKIP_DMG" = true ]; then
    exit 0
fi

# DMG: drag-to-Applications layout with a README including the macFUSE
# runtime requirement note.
DMG_ROOT="$DIST/dmg-root"
DMG_PATH="$DIST/Lumen-$VERSION.dmg"
rm -rf "$DMG_ROOT"
mkdir -p "$DMG_ROOT"
cp -R "$APP_ROOT" "$DMG_ROOT/"
printf 'Welcome to Lumen %s\n\nBefore first launch:\n1. Install macFUSE: brew install --cask macfuse (reboot after install)\n2. Create ~/Library/Application Support/Lumen/lumen.env per the getting-started docs\n3. Open Lumen.app - your drive mounts at /Volumes/Lumen\n' "$VERSION" > "$DMG_ROOT/README.txt"
rm -f "$DMG_PATH"
hdiutil create -volname "$APP_NAME" -srcfolder "$DMG_ROOT" -ov -format UDZO "$DMG_PATH"
echo "DMG complete: $DMG_PATH"

