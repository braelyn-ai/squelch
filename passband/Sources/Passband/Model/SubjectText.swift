// SUBJECT LINES ARE SOMEBODY ELSE'S TEXT. Every one of them was typed by a
// stranger, and this file holds the three things the app does about that: name
// the blank ones, flatten the multi-line ones, and — where a subject rides
// inside a prompt's data markers — make sure it cannot spell the marker that
// would close its own frame.
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
        text.count <= cap ? text : String(text.prefix(cap)) + "…"
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
