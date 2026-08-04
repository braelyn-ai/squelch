// "Somebody opened this" on the user's OWN sent mail — the read half of the
// tracking pixel the composer arms.
//
// The mark is deliberately soft. An open is a fetch of a 1x1 image, and Gmail
// fetches images through a proxy that can warm its cache before anybody has
// read anything; the copy says "via proxy" when that is what happened rather
// than claiming a read it cannot see. No opens means no mark at all — "not
// opened yet" and "never tracked" are the same silence, and both are normal.

import SwiftUI

enum ReadReceipts {
    /// How many lookups run at once. A thread is normally a handful of
    /// messages; a mailing-list monster is not a reason to open fifty sockets
    /// for an answer that is almost always empty.
    private static let batch = 8

    /// Every recorded open of each message that has one, keyed by local message
    /// id, oldest first.
    ///
    /// Asks about EVERY message in the thread rather than guessing which copy
    /// was the user's own: the wire carries no "this one is mine" flag, only a
    /// tracked send can have opens, and every other id answers with an empty
    /// list. Callers gate on the daemon having tracking configured, so a reader
    /// who does not use the feature spends nothing here.
    static func opens(for ids: [Int]) async -> [Int: [MessageOpen]] {
        var found: [Int: [MessageOpen]] = [:]
        for start in stride(from: 0, to: ids.count, by: batch) {
            let chunk = ids[start..<min(start + batch, ids.count)]
            await withTaskGroup(of: (Int, [MessageOpen]).self) { group in
                for id in chunk {
                    group.addTask {
                        // A failure is silence, not an error: the mark is an
                        // extra, and a thread must never fail to read for one.
                        (id, (try? await APIClient.shared.messageOpens(id)) ?? [])
                    }
                }
                for await (id, opens) in group where !opens.isEmpty {
                    found[id] = opens
                }
            }
        }
        return found
    }
}

/// The mark itself, rendered beside a sent message's header.
struct ReadReceiptMark: View {
    /// Every recorded open, oldest first. Empty renders nothing.
    let opens: [MessageOpen]

    var body: some View {
        if let latest = opens.last {
            Chip(text: label(latest), tone: Palette.inkFaint, symbol: "eye")
                .help(tooltip(latest))
        }
    }

    private func label(_ latest: MessageOpen) -> String {
        let age = Fmt.relAge(latest.date)
        let when = age == "now" ? "just now" : "\(age) ago"
        return latest.viaProxy ? "opened (via proxy) \(when)" : "opened \(when)"
    }

    /// The tooltip carries the exact stamp and the count the chip collapses,
    /// plus the caveat — the chip has no room to be honest at length.
    private func tooltip(_ latest: MessageOpen) -> String {
        let head =
            opens.count == 1
            ? "opened \(Fmt.dateTime(latest.date))"
            : "\(opens.count) opens · latest \(Fmt.dateTime(latest.date))"
        guard latest.viaProxy else { return head }
        return head
            + " — fetched by Gmail's image proxy, which sometimes loads images before "
            + "the recipient reads the message."
    }
}
