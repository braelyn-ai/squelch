#!/usr/bin/env bash
# Render the release-note table into markdown.
#
#   ./make-changelog.sh            regenerate ../docs/CHANGELOG.md
#   ./make-changelog.sh 0.0.4      print one release, for a GitHub release body
#   ./make-changelog.sh --check    fail if the committed CHANGELOG.md is stale
#
# The notes themselves live in Sources/Passband/Lib/ReleaseNotes.swift, which
# the app compiles directly for its What's New card. That is the point of the
# arrangement: the copy a human is shown in the app cannot be stale, and
# everything downstream of it is regenerated rather than maintained.
#
# swiftc for the same reason test.sh uses it: ReleaseNotes.swift is kept free of
# SwiftUI and Bundle, so a plain two-file compile is the whole build.

set -euo pipefail
cd "$(dirname "$0")"

BUILD=build/tools
OUT=../docs/CHANGELOG.md
mkdir -p "$BUILD"

xcrun swiftc -swift-version 6 -parse-as-library -Onone \
  -o "$BUILD/changelog" \
  Sources/Passband/Lib/ReleaseNotes.swift \
  Tools/ChangelogTool.swift

case "${1:-}" in
  --check)
    # For CI and for anyone about to tag: the table moved and the document did
    # not. Diffs rather than a bare exit code, because the fix is to look.
    if ! "$BUILD/changelog" | diff -u "$OUT" - ; then
      echo "==> $OUT is stale; run ./make-changelog.sh" >&2
      exit 1
    fi
    echo "==> $OUT is current"
    ;;
  "")
    "$BUILD/changelog" > "$OUT"
    echo "==> regenerated $OUT"
    ;;
  *)
    # One release to stdout. No file, no banner: the caller is piping this
    # straight into `gh release create --notes-file -`.
    "$BUILD/changelog" "$1"
    ;;
esac
