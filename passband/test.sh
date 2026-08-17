#!/usr/bin/env bash
# Run the Swift unit tests. swiftc-driven for the same reason build.sh is (see
# its header: xcodebuild on this machine is not to be trusted), and no XCTest
# because there is no test bundle host without it. Each suite is a @main
# executable compiled from PURE source files — the point of keeping the wire
# decoder synchronous and network-free is that it builds with just its data
# types: no app, no simulator, no key.
#
#   ./test.sh          build and run every suite

set -euo pipefail
cd "$(dirname "$0")"

BUILD=build/tests
mkdir -p "$BUILD"

run_suite() {
  local name="$1"
  shift
  echo "==> $name"
  xcrun swiftc -swift-version 6 -parse-as-library -Onone \
    -o "$BUILD/$name" "$@"
  "$BUILD/$name"
}

run_suite anthropic-stream \
  Sources/Passband/Assistant/JSONValue.swift \
  Sources/Passband/Assistant/AnthropicStream.swift \
  Tests/AnthropicStreamTests.swift

# Pairing pulls in Sessions.swift only because it builds its own URLSession
# there; nothing in this suite makes a request. Same payoff as above: the code
# normalizer and the deep-link parser are pure, so they test with no daemon.
run_suite pairing \
  Sources/Passband/Lib/Sessions.swift \
  Sources/Passband/Model/Pairing.swift \
  Tests/PairingTests.swift

# One file, no dependencies at all — SubjectText is kept that way on purpose.
# The marker sanitizer is what lets a stranger's subject line sit inside the
# assistant's system prompt, so it gets asserted rather than reasoned about.
run_suite subject-text \
  Sources/Passband/Model/SubjectText.swift \
  Tests/SubjectTextTests.swift

run_suite email-images \
  Sources/Passband/Lib/ImageProxy.swift \
  Tests/EmailImagesTests.swift

# The thread minimap's window math. CoreGraphics only — the rail that draws it
# is SwiftUI, the arithmetic that aims it is not.
run_suite minimap-geometry \
  Sources/Passband/Lib/MinimapGeometry.swift \
  Tests/MinimapGeometryTests.swift
