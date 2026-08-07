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
