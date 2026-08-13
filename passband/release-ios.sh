#!/bin/bash
# TestFlight upload for PassbandiOS. Archives Release, exports an App Store
# package, and uploads it when App Store Connect API credentials are present.
#
#   ./release-ios.sh            archive + export; upload if creds are set
#   ./release-ios.sh --no-upload  stop after export (upload via Xcode/Transporter)
#
# Credentials (optional; without them the script leaves an .ipa and says so):
#   SQUELCH_ASC_KEY_ID       App Store Connect API key id
#   SQUELCH_ASC_KEY_ISSUER   issuer id (App Store Connect > Users and Access > Keys)
#   SQUELCH_ASC_KEY_PATH     path to the AuthKey_<id>.p8
#
# The build number is the UTC minute: unique, monotonic, and derived from the
# clock instead of a state file some other checkout would fork. The marketing
# version is project.yml's MARKETING_VERSION unless SQUELCH_IOS_VERSION says
# otherwise — CI passes the ios-v* tag's version through it.
set -euo pipefail
cd "$(dirname "$0")"

TEAM_ID="AAT39BQ9LV"
BUILD="$(date -u +%Y%m%d%H%M)"
OUT="build/testflight"
ARCHIVE="$OUT/PassbandiOS.xcarchive"
EXPORT_PLIST="$OUT/export-options.plist"

# On a signed-in Mac, -allowProvisioningUpdates rides Xcode's session. Headless
# (CI), the same flag needs the App Store Connect API key handed to xcodebuild
# itself — cloud signing then mints/uses a managed distribution cert with no
# keychain on the runner.
AUTH=()
if [ -n "${SQUELCH_ASC_KEY_ID:-}" ] && [ -f "${SQUELCH_ASC_KEY_PATH:-}" ]; then
  AUTH=(-authenticationKeyPath "$(cd "$(dirname "$SQUELCH_ASC_KEY_PATH")" && pwd)/$(basename "$SQUELCH_ASC_KEY_PATH")"
        -authenticationKeyID "$SQUELCH_ASC_KEY_ID"
        -authenticationKeyIssuerID "$SQUELCH_ASC_KEY_ISSUER")
fi

VERSION_OVERRIDE=()
if [ -n "${SQUELCH_IOS_VERSION:-}" ]; then
  VERSION_OVERRIDE=(MARKETING_VERSION="$SQUELCH_IOS_VERSION")
fi

rm -rf "$OUT"
mkdir -p "$OUT"

echo "==> xcodegen"
xcodegen generate >/dev/null

echo "==> archiving ${SQUELCH_IOS_VERSION:-\$(project.yml)} build $BUILD"
xcodebuild -project Passband.xcodeproj -scheme PassbandiOS \
  -configuration Release -destination 'generic/platform=iOS' \
  archive -archivePath "$ARCHIVE" \
  CURRENT_PROJECT_VERSION="$BUILD" ${VERSION_OVERRIDE[@]+"${VERSION_OVERRIDE[@]}"} \
  -allowProvisioningUpdates ${AUTH[@]+"${AUTH[@]}"} -quiet

cat > "$EXPORT_PLIST" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>method</key>
	<string>app-store-connect</string>
	<key>teamID</key>
	<string>${TEAM_ID}</string>
</dict>
</plist>
PLIST

echo "==> exporting"
xcodebuild -exportArchive -archivePath "$ARCHIVE" \
  -exportOptionsPlist "$EXPORT_PLIST" -exportPath "$OUT" \
  -allowProvisioningUpdates ${AUTH[@]+"${AUTH[@]}"} -quiet

# Named for CFBundleDisplayName (Passband.ipa), not the target — glob so a
# rename never breaks the pipeline silently.
IPA="$(ls "$OUT"/*.ipa 2>/dev/null | head -1)"
[ -n "$IPA" ] || { echo "export produced no .ipa in $OUT" >&2; exit 1; }

if [ "${1:-}" = "--no-upload" ]; then
  echo "==> $IPA (upload skipped)"
  exit 0
fi

if [ -n "${SQUELCH_ASC_KEY_ID:-}" ] && [ -n "${SQUELCH_ASC_KEY_ISSUER:-}" ] && [ -f "${SQUELCH_ASC_KEY_PATH:-}" ]; then
  echo "==> uploading build $BUILD"
  # altool takes no key path — it searches a few blessed directories for
  # AuthKey_<id>.p8, so place a copy where it looks.
  mkdir -p "$HOME/.appstoreconnect/private_keys"
  cp -f "$SQUELCH_ASC_KEY_PATH" "$HOME/.appstoreconnect/private_keys/AuthKey_${SQUELCH_ASC_KEY_ID}.p8"
  xcrun altool --upload-app -f "$IPA" -t ios \
    --apiKey "$SQUELCH_ASC_KEY_ID" --apiIssuer "$SQUELCH_ASC_KEY_ISSUER"
  echo "==> uploaded; App Store Connect processes it in ~10-30 min"
else
  echo "==> $IPA"
  echo "    No ASC API key in the environment — upload it with Xcode's
    Organizer (Window > Organizer > Archives) or the Transporter app,
    or set SQUELCH_ASC_KEY_ID / SQUELCH_ASC_KEY_ISSUER / SQUELCH_ASC_KEY_PATH."
fi
