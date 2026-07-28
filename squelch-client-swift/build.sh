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

# shellcheck source=signing.sh
. ./signing.sh

MODE="${1:-debug}"
APP="build/Squelch.app"
SRC_DIR="Sources/Squelch"
RES_DIR="$SRC_DIR/Resources"

# Versioning. ./VERSION is the single source of truth for the user-facing
# version; keep project.yml's MARKETING_VERSION in step with it. The build
# number is the commit count, which is monotonic across the whole history —
# macOS and every updater require CFBundleVersion to only ever increase. Both
# are env-overridable so a release can be cut from a dirty or exported tree.
MARKETING_VERSION="${MARKETING_VERSION:-$(cat VERSION)}"
BUILD_NUMBER="${BUILD_NUMBER:-$(git rev-list --count HEAD 2>/dev/null || echo 1)}"

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

echo "==> compiling ($MODE) v$MARKETING_VERSION build $BUILD_NUMBER"
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
    -e "s/\\\$(MARKETING_VERSION)/$MARKETING_VERSION/g" \
    -e "s/\\\$(CURRENT_PROJECT_VERSION)/$BUILD_NUMBER/g" \
    -e 's/\$(MACOSX_DEPLOYMENT_TARGET)/26.0/g' \
    "$SRC_DIR/Info.plist" > "$APP/Contents/Info.plist"

cp -R "$RES_DIR/." "$APP/Contents/Resources/" 2>/dev/null || true
printf 'APPL????' > "$APP/Contents/PkgInfo"

# Sign with the Developer ID when the machine has one, ad-hoc otherwise.
#
# This is what stops the keychain re-prompting on every dev build. Keychain
# ACLs match against a bundle's designated requirement, and an ad-hoc
# signature's requirement is a cdhash of the build itself — it changes on every
# recompile, so each build looks like a brand new app and has to ask again. A
# Developer ID requirement is keyed to the team and never changes, so the ACL
# keeps matching for the life of the certificate. Ad-hoc remains the fallback
# so the build still works on a machine with no certificate.
ENTITLEMENTS="$SRC_DIR/Squelch.entitlements"
if [ -z "${SIGN_ID:-}" ]; then
  SIGN_ID="$(detect_sign_identity)"
  SIGN_ID="${SIGN_ID:--}"
fi

CODESIGN_FLAGS=(--force)
SIGN_NOTE=""
if [ "${HARDENED:-0}" = "1" ]; then
  # Hardened runtime is mandatory for notarization; the secure timestamp keeps
  # the signature valid after the signing certificate expires.
  CODESIGN_FLAGS+=(--options runtime --timestamp)
  SIGN_NOTE=" (hardened runtime)"
else
  # Local builds need get-task-allow so a debugger can attach. Notarization
  # rejects that entitlement outright, so it is injected into a throwaway copy
  # and can never reach a release build.
  ENTITLEMENTS="build/Squelch.debug.entitlements"
  cp "$SRC_DIR/Squelch.entitlements" "$ENTITLEMENTS"
  /usr/libexec/PlistBuddy -c \
    'Add :com.apple.security.get-task-allow bool true' "$ENTITLEMENTS" >/dev/null
fi

echo "==> signing as $SIGN_ID$SIGN_NOTE"
codesign "${CODESIGN_FLAGS[@]}" --sign "$SIGN_ID" --entitlements "$ENTITLEMENTS" "$APP"

echo "==> built $APP"

if [ "$MODE" = "run" ]; then
  echo "==> launching"
  open "$APP"
fi
