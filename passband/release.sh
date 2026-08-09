#!/usr/bin/env bash
# Cut a Passband release, end to end: notarized build, GitHub release under
# the passband-v tag, regenerated Sparkle appcast, site push, Homebrew cask
# bump. RELEASING.md describes the same steps; this script IS the ritual.
#
#   ./release.sh          release $(cat VERSION)
#   ./release.sh --dry    everything except publish (no release, no pushes)
#
# Prerequisites (all one-time, all this machine): Developer ID cert, the
# passband-notary notarytool profile, the Sparkle EdDSA key in the login
# keychain, gh authed as someone who can write braelyn-ai/squelch and
# braelyn-ai/homebrew-tap.

set -euo pipefail
cd "$(dirname "$0")"

DRY=0
[ "${1:-}" = "--dry" ] && DRY=1

VERSION=$(cat VERSION)
TAG="passband-v$VERSION"
ZIP="build/Passband-$VERSION.zip"
SITE_DIR="../passband-site"
TAP_REPO="braelyn-ai/homebrew-tap"

# ------------------------------------------------------------- preflight ---
# A release is cut from committed history: the build number is the commit
# count, so uncommitted changes would ship code that no commit describes.
# Scoped to the paths that enter the artifact and the feed — this is a
# monorepo, and daemon work in flight must not hold a client release
# hostage. A dry run publishes nothing, so it may rehearse from dirt.
if [ "$DRY" = 0 ] \
  && [ -n "$(git status --porcelain --untracked-files=no -- . ../passband-site)" ]; then
  echo "error: passband/ or passband-site/ not clean; commit or stash first" >&2
  exit 1
fi
if gh release view "$TAG" >/dev/null 2>&1; then
  echo "error: release $TAG already exists; bump VERSION first" >&2
  exit 1
fi
# project.yml mirrors VERSION by hand; a release is where drift would ship.
if ! grep -q "MARKETING_VERSION: \"$VERSION\"" project.yml; then
  echo "error: project.yml MARKETING_VERSION != $VERSION; keep them in step" >&2
  exit 1
fi

# ----------------------------------------------------------------- build ---
if [ "$DRY" = 1 ]; then
  # Sign-only: exercises everything local without notary credentials or
  # publishing anything.
  ./build-release.sh --no-notary
  echo "==> dry run: would notarize, publish $TAG, regen appcast, bump cask"
  exit 0
fi
./build-release.sh

echo "==> creating GitHub release $TAG"
gh release create "$TAG" "$ZIP" \
  --title "Passband $VERSION" \
  --notes "Passband $VERSION. Auto-update will offer this to existing installs; new installs: unzip and drag to Applications, or \`brew install braelyn-ai/tap/passband\`."

echo "==> regenerating appcast"
./make-appcast.sh

echo "==> publishing appcast"
git -C "$SITE_DIR" add appcast.xml
git -C "$SITE_DIR" commit -m "passband-site: appcast for Passband $VERSION"
git -C "$SITE_DIR" push

# ------------------------------------------------------------------ cask ---
# The cask pins the zip's checksum; compute it from the exact artifact the
# release carries.
SHA=$(shasum -a 256 "$ZIP" | cut -d' ' -f1)
echo "==> bumping Homebrew cask (sha256 $SHA)"
TAP_TMP=$(mktemp -d)
gh repo clone "$TAP_REPO" "$TAP_TMP" -- --depth 1 >/dev/null 2>&1
sed -i '' \
  -e "s/^  version \".*\"/  version \"$VERSION\"/" \
  -e "s/^  sha256 \".*\"/  sha256 \"$SHA\"/" \
  "$TAP_TMP/Casks/passband.rb"
git -C "$TAP_TMP" commit -am "passband $VERSION"
git -C "$TAP_TMP" push
rm -rf "$TAP_TMP"

echo
echo "==> released Passband $VERSION"
echo "    verify: curl -s https://passband.app/appcast.xml | grep $VERSION"
