#!/usr/bin/env bash
# Regenerate the Sparkle appcast from the released archives and stage it into
# passband-site (deploying the site publishes the feed).
#
# releases/ accumulates one zip per shipped version and is the generator's
# working set: entries exist for exactly the zips present. It is gitignored;
# losing it only loses old feed entries, not the updates themselves (those
# live on GitHub releases).
#
# EdDSA signing uses the private key in the login keychain (created by
# vendor/Sparkle/bin/generate_keys). No deltas for now: full zips are ~10 MB
# and delta filenames would defeat the /download/ redirect's tag mapping.

set -euo pipefail
cd "$(dirname "$0")"

./vendor/fetch-sparkle.sh
mkdir -p releases

# Stage the freshly built archive, if build-release.sh left one.
for zip in build/Passband-*.zip; do
  if [ -e "$zip" ]; then cp -f "$zip" releases/; fi
done

# Signing key: the login keychain by default (this machine), or a key file
# when SPARKLE_ED_KEY_FILE is set (CI, where the key arrives as a secret).
KEY_ARGS=()
if [ -n "${SPARKLE_ED_KEY_FILE:-}" ]; then
  KEY_ARGS=(--ed-key-file "$SPARKLE_ED_KEY_FILE")
fi

# (macOS bash 3.2: expanding an empty array under set -u is an error, hence
# the ${arr[@]+...} guard.)
./vendor/Sparkle/bin/generate_appcast releases \
  ${KEY_ARGS[@]+"${KEY_ARGS[@]}"} \
  --download-url-prefix "https://passband.app/download/" \
  --maximum-deltas 0 \
  -o ../passband-site/appcast.xml

echo "==> ../passband-site/appcast.xml regenerated from $(ls releases/*.zip | wc -l | tr -d ' ') archive(s)"
echo "    commit and push passband-site to publish"
