// RANKING — the pure scorer for the Sitrep "For your eyes" zone.
//
// A standing item is scored by a configurable blend of URGENCY (how soon it's
// due) and SEVERITY (how important it is):
//
//   urgency(u)  = 1.0                      if overdue (deadline in the past)
//               = 1 / (1 + daysUntilDue/7) if it has a future deadline
//               = 0                        if it has no deadline
//   severity(u) = importance / 100         (clamped to 0..1)
//   score       = w * urgency + (1 - w) * severity     // w = rankWeight
//
// Ported from squelch-desktop/src/lib/ranking.ts.

import Foundation

/// The default blend weight when no preference is set (time-leaning).
let defaultRankWeight: Double = 0.6

private func clamp01(_ n: Double) -> Double {
    if n.isNaN { return 0 }
    return min(1, max(0, n))
}

enum Ranking {
    /// Urgency in 0..1. Overdue pins to 1.0; a future deadline decays with
    /// distance (half-life-ish at a week); no/invalid deadline is 0.
    static func urgency(_ item: AttentionUpdate, now: Date = Date()) -> Double {
        guard let t = Fmt.date(item.deadline) else { return 0 }
        if t <= now { return 1 }
        let daysUntilDue = t.timeIntervalSince(now) / 86400
        return 1 / (1 + daysUntilDue / 7)
    }

    /// Severity in 0..1 from importance (0..100).
    static func severity(_ item: AttentionUpdate) -> Double {
        clamp01(Double(item.importance) / 100)
    }

    /// Blended score in 0..1. `weight` (0..1) is the urgency share.
    static func score(_ item: AttentionUpdate, weight: Double = defaultRankWeight, now: Date = Date())
        -> Double
    {
        let w = clamp01(weight)
        return w * urgency(item, now: now) + (1 - w) * severity(item)
    }

    /// A NEW array sorted by descending score. Stable: equal scores keep their
    /// original relative order so a re-render never reshuffles ties.
    static func rank(
        _ items: [AttentionUpdate], weight: Double = defaultRankWeight, now: Date = Date()
    ) -> [AttentionUpdate] {
        items.enumerated()
            .map { (index: $0.offset, item: $0.element, s: score($0.element, weight: weight, now: now)) }
            .sorted { a, b in a.s != b.s ? a.s > b.s : a.index < b.index }
            .map(\.item)
    }
}
