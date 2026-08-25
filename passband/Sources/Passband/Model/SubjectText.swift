// SUBJECT LINES ARE SOMEBODY ELSE'S TEXT. Every one of them was typed by a
// stranger, and this file holds the four things the app does about that: name
// the blank ones, flatten the multi-line ones, drop the decoration a marketer
// put in front of them, and — where a subject rides inside a prompt's data
// markers — make sure it cannot spell the marker that would close its own frame.
//
// Deliberately Foundation-only and free of app types, so `test.sh` can compile
// it on its own next to Tests/SubjectTextTests.swift. The marker rule is the
// reason that suite exists: it is a security boundary, and it is one function.

import Foundation

extension String {
    /// The subject as a human should see it. Blank-aware rather than merely
    /// empty-aware: a subject of spaces (or of one non-breaking space, which is
    /// what a mailer that "cleared" the field often sends) renders as nothing at
    /// all, and a row with nothing on it reads as a bug.
    var displaySubject: String {
        contains { !$0.isWhitespace } ? self : "(no subject)"
    }

    /// One line, bounded: whitespace runs collapse to single spaces and the
    /// result is cut at `cap` with an ellipsis. What card previews and snippets
    /// want — a body's own newlines are its formatting, not the caller's.
    func flattenedLine(cap: Int) -> String {
        Self.capped(Self.flattened(self), cap: cap)
    }

    /// The line with its EMOJI REMOVED. Only for text the app re-uses as a
    /// LABEL — a shipment's item name, lifted out of "🚚 Your order from …" —
    /// never for a subject shown as a subject, where the sender's decoration is
    /// part of what they wrote.
    ///
    /// Presentation, not the `isEmoji` property, decides. That property is true
    /// of `#`, `*` and every ASCII digit (they are emoji BASES, waiting on a
    /// variation selector), so stripping on it alone would eat the "5" out of
    /// "5 Port USB Hub". A scalar goes only when it renders as emoji by default
    /// or when its own cluster carries the U+FE0F that makes it render that way.
    /// Whole GRAPHEME CLUSTERS go at once, which is what takes a ZWJ family or a
    /// skin-toned hand out in one piece instead of leaving its joiners behind.
    var withoutEmoji: String {
        Self.flattened(String(filter { !$0.isEmojiCluster }))
    }

    /// The line as a PROMPT may state it inside data markers: flattened, with
    /// every bracket run long enough to spell a marker collapsed away, then
    /// capped. nil when nothing survives, which is the caller's cue to say
    /// nothing rather than to frame an empty line.
    ///
    /// COLLAPSE BEFORE TRUNCATION, or the cut itself could leave a run standing
    /// at the very end where the closing marker goes.
    func markerSafeLine(cap: Int) -> String? {
        let safe = Self.collapsingMarkerRuns(Self.flattened(self))
        return safe.isEmpty ? nil : Self.capped(safe, cap: cap)
    }

    private static func flattened(_ text: String) -> String {
        text.split(whereSeparator: \.isWhitespace).joined(separator: " ")
    }

    private static func capped(_ text: String, cap: Int) -> String {
        // SCALARS, not Characters: the cap bounds what the prompt pays for by
        // the token, and one grapheme can carry an unbounded pile of combining
        // marks. A cut mid-cluster costs a preview an accent; an uncut cluster
        // costs the prompt five thousand scalars per request.
        let scalars = text.unicodeScalars
        guard scalars.count > cap else { return text }
        return String(String.UnicodeScalarView(scalars.prefix(cap))) + "…"
    }

    /// Collapse every run of three or more identical `<` or `>` to a single
    /// one, in ONE pass over the UNICODE SCALARS.
    ///
    /// Both halves of that are load-bearing. Scalars, because Swift's string
    /// search is grapheme-based with canonical equivalence: `">>>\u{0301}"` is
    /// three scalars but only two Characters (the last `>` and the combining
    /// acute are one cluster), so a Character-level search for `">>>"` does not
    /// find it — while the bytes that reach the model spell the marker exactly.
    /// One pass, because a loop of replacements is a different function: it has
    /// to run until it converges, and one that stopped early would leave `"<<<"`
    /// standing inside `"<<<<<"`.
    private static func collapsingMarkerRuns(_ text: String) -> String {
        var out = String.UnicodeScalarView()
        var run: Unicode.Scalar? = nil
        var count = 0

        func flush() {
            guard let scalar = run else { return }
            // Two is not a marker and never becomes one — leave short runs
            // exactly as they arrived.
            for _ in 0..<(count >= 3 ? 1 : count) { out.append(scalar) }
            run = nil
            count = 0
        }

        for scalar in text.unicodeScalars {
            // Format characters (ZWSP, ZWJ, word joiners, bidi controls) are
            // invisible ink: parked inside a bracket run they split it without
            // showing anything, and a line that RENDERS as the marker is close
            // enough for a model that pattern-matches the frame. All of Cf is
            // dropped from the prompt line — and dropped WITHOUT flushing, so
            // the run one tried to split is judged whole. (An emoji family in a
            // subject loses its joiners here; the prompt can live with that.)
            if scalar.properties.generalCategory == .format { continue }
            if scalar == "<" || scalar == ">" {
                if scalar == run {
                    count += 1
                } else {
                    flush()
                    run = scalar
                    count = 1
                }
                continue
            }
            flush()
            out.append(scalar)
        }
        flush()
        return String(out)
    }
}

extension Character {
    /// Does this grapheme cluster RENDER as emoji? See `withoutEmoji` for why
    /// the default-presentation test and the explicit U+FE0F test are both
    /// needed and why neither `isEmoji` alone would do.
    fileprivate var isEmojiCluster: Bool {
        guard let first = unicodeScalars.first else { return false }
        if first.properties.isEmojiPresentation { return true }
        return first.properties.isEmoji && unicodeScalars.contains("\u{FE0F}")
    }
}
