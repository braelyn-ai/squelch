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

# The changelog and the rule that decides which of it a given install is owed.
# One file, no dependencies: ReleaseNotes.swift is kept free of SwiftUI and
# Bundle precisely so this suite and make-changelog.sh can both compile it
# alone, and so the running version arrives as an argument rather than as
# whatever bundle the binary happens to sit in.
run_suite release-notes \
  Sources/Passband/Lib/ReleaseNotes.swift \
  Tests/ReleaseNotesTests.swift

# One file, no dependencies at all — SubjectText is kept that way on purpose.
# The marker sanitizer is what lets a stranger's subject line sit inside the
# assistant's system prompt, so it gets asserted rather than reasoned about.
run_suite subject-text \
  Sources/Passband/Model/SubjectText.swift \
  Tests/SubjectTextTests.swift

# The attachment buckets against the wire type they bucket. WireTypes is pure
# Codable structs, so the pair builds with no app and no daemon.
run_suite attachment-kinds \
  Sources/Passband/Model/WireTypes.swift \
  Sources/Passband/Lib/AttachmentKinds.swift \
  Tests/AttachmentKindsTests.swift

# Where a stranger's filename lands on disk, and which renderer draws it. The
# staging rules are the last thing standing between an attachment named
# `invoice.html` and WebKit, so they are asserted rather than read.
run_suite staged-attachment \
  Sources/Passband/Model/WireTypes.swift \
  Sources/Passband/Lib/AttachmentKinds.swift \
  Sources/Passband/Lib/StagedAttachment.swift \
  Tests/StagedAttachmentTests.swift

run_suite email-images \
  Sources/Passband/Lib/ImageProxy.swift \
  Tests/EmailImagesTests.swift

# The OTHER image rewrite: `cid:` references to this message's own parts. Pure
# string and url work, so it builds with the wire type and the buckets it gates
# on — no WebKit, no daemon. MailCSP rides along because the policy those urls
# are loaded under is the one line of the reading frame that can be asserted
# without standing a content process up, and Trackers because it runs FIRST in
# the real pipeline: what it takes out of a body is what neither answer here may
# still claim.
run_suite cid-images \
  Sources/Passband/Model/WireTypes.swift \
  Sources/Passband/Lib/AttachmentKinds.swift \
  Sources/Passband/Lib/HTMLImg.swift \
  Sources/Passband/Lib/ImageProxy.swift \
  Sources/Passband/Lib/Trackers.swift \
  Sources/Passband/Lib/CidImages.swift \
  Sources/Passband/Lib/MailCSP.swift \
  Tests/CidImagesTests.swift

# The thread minimap's window math. CoreGraphics only — the rail that draws it
# is SwiftUI, the arithmetic that aims it is not. ThreadStyle comes along
# because a bubble is a narrower measure than a card, which the guess has to
# know; the enum is kept free of SwiftUI and of the account for exactly this.
run_suite minimap-geometry \
  Sources/Passband/Lib/MinimapGeometry.swift \
  Sources/Passband/Lib/ThreadStyle.swift \
  Tests/MinimapGeometryTests.swift

# The automatic thread style: a guess about somebody's mail, so it is asserted
# fixture by fixture rather than reasoned about. Quotes comes along because the
# length test is fed by the quote splitter — a reply under forty lines of chain
# is a short message — and both files are pure Foundation for this reason.
run_suite thread-style \
  Sources/Passband/Lib/ThreadStyle.swift \
  Sources/Passband/Lib/Quotes.swift \
  Tests/ThreadStyleTests.swift

# The banking card's recency window (issue #82): 24h or since-last-open,
# whichever reaches further back. Platform rides along for the notification
# names the lifecycle plumbing observes.
run_suite sitrep-window \
  Sources/Passband/Lib/Platform.swift \
  Sources/Passband/Lib/SitrepWindow.swift \
  Tests/SitrepWindowTests.swift

# When the two-week ask fires, and every way it must not. Same shape as the
# window rule above: the decision is a pure static, so it tests without a
# daemon, a clock, or a UserDefaults this suite would have to clean up after.
run_suite share-nudge \
  Sources/Passband/Lib/Platform.swift \
  Sources/Passband/Lib/ShareNudge.swift \
  Tests/ShareNudgeTests.swift

# What "remind me…" resolves to. Foundation only, and every date computed
# through an injected now/calendar — which is the whole reason it is testable:
# a reminder is only ever wrong LATER, so the arithmetic has to be pinned here
# rather than discovered by an email that never came back.
run_suite remind-times \
  Sources/Passband/Lib/RemindTimes.swift \
  Tests/RemindTimesTests.swift

# Which senders leave the device. `eligibleFaviconDomain` is the privacy
# boundary in SenderIdentity — a human correspondent answers nil and that nil
# is why the correspondent graph stays local — so the brand/robot heuristics
# guarding it are asserted rather than reasoned about. Pure string work;
# WireTypes and Format ride along for the Tier and the capitalizer.
# The wire contract behind the disconnected banner, and the RFC3339 shapes its
# since-when has to survive. WireTypes carries the object; Format carries the
# parser that tries both fractional and plain, which is the bug this pins.
run_suite gmail-health \
  Sources/Passband/Model/WireTypes.swift \
  Sources/Passband/Lib/Format.swift \
  Sources/Passband/Lib/AsyncMemo.swift \
  Tests/GmailHealthTests.swift

run_suite sender-identity \
  Sources/Passband/Model/WireTypes.swift \
  Sources/Passband/Lib/Format.swift \
  Sources/Passband/Lib/AsyncMemo.swift \
  Sources/Passband/Lib/SenderIdentity.swift \
  Tests/SenderIdentityTests.swift
