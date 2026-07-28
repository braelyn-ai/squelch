#!/usr/bin/env bash
# Build Squelch.app straight from the command line with swiftc — no xcodebuild,
# no IDE. This is the canonical build for this repo because the Xcode CLI on
# this machine intermittently fails to load its simulator plug-ins, which
# xcodebuild treats as fatal even for a macOS-only target. `swiftc` itself is
# unaffected, so we drive it directly and assemble the bundle ourselves.
#
#   ./build.sh          debug build
#   ./build.sh release  optimized build
#   ./build.sh run      build, then launch the app
#
# The .xcodeproj (via `xcodegen generate`) is still checked in and is the
# preferred path when xcodebuild is healthy; both produce the same bundle.

set -euo pipefail

cd "$(dirname "$0")"

MODE="${1:-debug}"
APP="build/Squelch.app"
SRC_DIR="Sources/Squelch"
RES_DIR="$SRC_DIR/Resources"

SWIFT_FLAGS=(
  -target arm64-apple-macosx26.0
  -swift-version 6
  -strict-concurrency=minimal
  -framework AppKit
  -framework SwiftUI
  -framework WebKit
  -framework Security
  -framework UniformTypeIdentifiers
  -framework PDFKit
)

case "$MODE" in
  release) SWIFT_FLAGS+=(-O -whole-module-optimization) ;;
  *)       SWIFT_FLAGS+=(-Onone -g) ;;
esac

echo "==> compiling ($MODE)"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

# Every .swift under Sources/Squelch, excluding the resources folder.
# (`mapfile` is bash 4+; macOS ships bash 3.2, so read the list the portable way.)
SOURCES=()
while IFS= read -r file; do
  SOURCES+=("$file")
done < <(find "$SRC_DIR" -name '*.swift' -not -path "$RES_DIR/*" | sort)
echo "    ${#SOURCES[@]} source files"

xcrun swiftc "${SWIFT_FLAGS[@]}" -o "$APP/Contents/MacOS/Squelch" "${SOURCES[@]}"

echo "==> assembling bundle"
# Info.plist with the build-setting placeholders resolved.
sed -e 's/\$(EXECUTABLE_NAME)/Squelch/g' \
    -e 's/\$(PRODUCT_BUNDLE_IDENTIFIER)/dev.squelch.client/g' \
    -e 's/\$(PRODUCT_NAME)/Squelch/g' \
    -e 's/\$(MARKETING_VERSION)/1.0/g' \
    -e 's/\$(CURRENT_PROJECT_VERSION)/1/g' \
    -e 's/\$(MACOSX_DEPLOYMENT_TARGET)/26.0/g' \
    "$SRC_DIR/Info.plist" > "$APP/Contents/Info.plist"

cp -R "$RES_DIR/." "$APP/Contents/Resources/" 2>/dev/null || true
printf 'APPL????' > "$APP/Contents/PkgInfo"

# Ad-hoc sign so the keychain identifies the app consistently across launches
# (an unsigned binary gets a fresh identity each build and re-prompts).
codesign --force --sign - --entitlements "$SRC_DIR/Squelch.entitlements" "$APP" 2>/dev/null \
  || codesign --force --sign - "$APP"

echo "==> built $APP"

if [ "$MODE" = "run" ]; then
  echo "==> launching"
  open "$APP"
fi
