#!/usr/bin/env bash
# Build a distributable Squelch.app: Developer ID signature, hardened runtime,
# Apple notarization, stapled ticket. The result opens by double-click on any
# Mac; an ad-hoc build from plain ./build.sh does not.
#
#   ./build-release.sh              sign, notarize, staple, package
#   ./build-release.sh --no-notary  sign only (fast; still Gatekeeper-warned)
#
# One-time setup — store an app-specific password from appleid.apple.com under
# a keychain profile that this script reads by name:
#
#   xcrun notarytool store-credentials squelch-notary \
#     --apple-id <your apple id> --team-id <your team id>
#
# Overridable: SIGN_ID (defaults to the Developer ID Application cert found in
# the keychain) and NOTARY_PROFILE (defaults to squelch-notary).

set -euo pipefail

cd "$(dirname "$0")"

NOTARIZE=1
[ "${1:-}" = "--no-notary" ] && NOTARIZE=0

APP="build/Squelch.app"
NOTARY_PROFILE="${NOTARY_PROFILE:-squelch-notary}"

# ---------------------------------------------------------------- identity ---
# Fall back to whatever Developer ID Application cert is in the keychain, so a
# second machine or a second developer needs no edits here. Unlike build.sh
# there is no ad-hoc fallback: an unsigned release is never what anyone wants.
. ./signing.sh
SIGN_ID="${SIGN_ID:-$(detect_sign_identity)}"

if [ -z "$SIGN_ID" ]; then
  echo "error: no 'Developer ID Application' identity in the keychain." >&2
  echo "       Create one at developer.apple.com (Certificates > +) and" >&2
  echo "       download it, or set SIGN_ID to an identity name." >&2
  exit 1
fi

# --------------------------------------------------------------- build/sign ---
# HARDENED turns on the runtime and the secure timestamp, both required by the
# notary service and both absent from local builds so debugging still works.
SIGN_ID="$SIGN_ID" HARDENED=1 ./build.sh release

VERSION=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$APP/Contents/Info.plist")
BUILD_NUM=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleVersion' "$APP/Contents/Info.plist")
STEM="Squelch-$VERSION"

echo "==> verifying signature"
codesign --verify --strict --verbose=2 "$APP"

if [ "$NOTARIZE" -eq 0 ]; then
  echo "==> signed but NOT notarized — Gatekeeper will still warn on other Macs"
  exit 0
fi

# ---------------------------------------------------------------- notarize ---
# Notarization takes a container, never a bare bundle. This zip is a throwaway
# used only for the upload; the shipping archive is rebuilt after stapling.
ZIP="build/$STEM-notarize.zip"
echo "==> packaging for notarization"
rm -f "$ZIP"
ditto -c -k --keepParent "$APP" "$ZIP"

echo "==> submitting to Apple (typically 2-15 min)"
SUBMIT_LOG="build/notarytool-submit.txt"
set +e
xcrun notarytool submit "$ZIP" \
  --keychain-profile "$NOTARY_PROFILE" \
  --wait 2>&1 | tee "$SUBMIT_LOG"
SUBMIT_STATUS=${PIPESTATUS[0]}
set -e

if [ "$SUBMIT_STATUS" -ne 0 ] || grep -q 'status: Invalid' "$SUBMIT_LOG"; then
  echo "==> notarization failed; fetching the rejection log" >&2
  SUBMISSION_ID=$(grep -m1 -Eo '[0-9a-f]{8}-([0-9a-f]{4}-){3}[0-9a-f]{12}' "$SUBMIT_LOG" || true)
  if [ -n "$SUBMISSION_ID" ]; then
    xcrun notarytool log "$SUBMISSION_ID" --keychain-profile "$NOTARY_PROFILE" >&2 || true
  fi
  exit 1
fi

# The ticket is stapled into the .app so a first launch works offline. stapler
# cannot write into a zip, which is why the shipping archive comes after.
echo "==> stapling ticket"
xcrun stapler staple "$APP"
rm -f "$ZIP"

# ----------------------------------------------------------------- package ---
ARCHIVE="build/$STEM.zip"
echo "==> packaging $ARCHIVE"
rm -f "$ARCHIVE"
ditto -c -k --keepParent "$APP" "$ARCHIVE"

echo "==> verifying Gatekeeper acceptance"
spctl -a -vvv -t exec "$APP"

echo
echo "==> $STEM (build $BUILD_NUM) ready: $ARCHIVE"
