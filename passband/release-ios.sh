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
# version stays project.yml's MARKETING_VERSION.
set -euo pipefail
cd "$(dirname "$0")"

TEAM_ID="AAT39BQ9LV"
BUILD="$(date -u +%Y%m%d%H%M)"
OUT="build/testflight"
ARCHIVE="$OUT/PassbandiOS.xcarchive"
EXPORT_PLIST="$OUT/export-options.plist"

rm -rf "$OUT"
mkdir -p "$OUT"

echo "==> xcodegen"
xcodegen generate >/dev/null

echo "==> archiving build $BUILD"
xcodebuild -project Passband.xcodeproj -scheme PassbandiOS \
  -configuration Release -destination 'generic/platform=iOS' \
  archive -archivePath "$ARCHIVE" \
  CURRENT_PROJECT_VERSION="$BUILD" \
  -allowProvisioningUpdates -quiet

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
  -allowProvisioningUpdates -quiet

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
  # altool wants the key under ./private_keys or a few blessed dirs; point it
  # at the key's own directory instead of copying a credential around.
  xcrun altool --upload-app -f "$IPA" -t ios \
    --apiKey "$SQUELCH_ASC_KEY_ID" --apiIssuer "$SQUELCH_ASC_KEY_ISSUER" \
    --private-key-path "$SQUELCH_ASC_KEY_PATH"
  echo "==> uploaded; App Store Connect processes it in ~10-30 min"
else
  echo "==> $IPA"
  echo "    No ASC API key in the environment — upload it with Xcode's
    Organizer (Window > Organizer > Archives) or the Transporter app,
    or set SQUELCH_ASC_KEY_ID / SQUELCH_ASC_KEY_ISSUER / SQUELCH_ASC_KEY_PATH."
fi
