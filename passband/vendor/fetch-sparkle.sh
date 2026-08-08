#!/usr/bin/env bash
# Fetch the pinned Sparkle release into vendor/Sparkle. build.sh calls this
# when the framework is missing, so a fresh clone builds without any manual
# step. Sparkle is vendored as the PREBUILT framework rather than a package
# dependency because build.sh compiles a flat swiftc file list with no package
# resolution (same reason the PostHog client is hand-written).
#
# The checksum pins the exact artifact: a re-tagged release or a tampered
# download fails loudly instead of shipping an updater we didn't audit. Bump
# VERSION and SHA256 together.

set -euo pipefail
cd "$(dirname "$0")"

VERSION="2.9.5"
SHA256="015336b601493e05c237964954bff6191370003d94edefe663724c88840d73cc"
TARBALL="Sparkle-$VERSION.tar.xz"
URL="https://github.com/sparkle-project/Sparkle/releases/download/$VERSION/$TARBALL"

if [ -f "Sparkle/Sparkle.framework/Versions/B/Sparkle" ] \
  && [ "$(cat Sparkle/.version 2>/dev/null)" = "$VERSION" ]; then
  exit 0
fi

echo "==> fetching Sparkle $VERSION"
curl -sL -o "$TARBALL" "$URL"
echo "$SHA256  $TARBALL" | shasum -a 256 -c - >/dev/null

rm -rf Sparkle
mkdir -p Sparkle
tar -xf "$TARBALL" -C Sparkle
rm -f "$TARBALL"
echo "$VERSION" > Sparkle/.version
echo "==> Sparkle $VERSION vendored"
