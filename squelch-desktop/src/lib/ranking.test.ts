// Pure-logic tests for the "For your eyes" ranking scorer. Run: `bun test`.

import { expect, test, describe } from "bun:test";
import {
  urgency,
  severity,
  score,
  rankItems,
  DEFAULT_RANK_WEIGHT,
  type Rankable,
} from "./ranking";

const NOW = Date.UTC(2026, 6, 22, 12, 0, 0); // fixed clock for determinism

function at(offsetDays: number): string {
  return new Date(NOW + offsetDays * 86_400_000).toISOString();
}

function item(p: Partial<Rankable>): Rankable {
  return { deadline: p.deadline ?? null, importance: p.importance ?? 0 };
}

describe("urgency", () => {
  test("no deadline is 0", () => {
    expect(urgency(item({ deadline: null }), NOW)).toBe(0);
  });

  test("invalid deadline is 0", () => {
    expect(urgency(item({ deadline: "not-a-date" }), NOW)).toBe(0);
  });

  test("overdue pins to 1.0", () => {
    expect(urgency(item({ deadline: at(-1) }), NOW)).toBe(1);
    expect(urgency(item({ deadline: at(-30) }), NOW)).toBe(1);
  });

  test("deadline exactly now counts as overdue (1.0)", () => {
    expect(urgency(item({ deadline: at(0) }), NOW)).toBe(1);
  });

  test("one week out is 0.5 (half-life at a week)", () => {
    expect(urgency(item({ deadline: at(7) }), NOW)).toBeCloseTo(0.5, 10);
  });

  test("further out decays toward 0 but stays positive", () => {
    const near = urgency(item({ deadline: at(3) }), NOW);
    const far = urgency(item({ deadline: at(30) }), NOW);
    expect(near).toBeGreaterThan(far);
    expect(far).toBeGreaterThan(0);
    expect(near).toBeLessThan(1);
  });
});

describe("severity", () => {
  test("maps importance/100", () => {
    expect(severity(item({ importance: 0 }))).toBe(0);
    expect(severity(item({ importance: 50 }))).toBe(0.5);
    expect(severity(item({ importance: 100 }))).toBe(1);
  });

  test("clamps out-of-range importance", () => {
    expect(severity(item({ importance: -20 }))).toBe(0);
    expect(severity(item({ importance: 150 }))).toBe(1);
  });
});

describe("score", () => {
  test("default weight blends 0.6 urgency + 0.4 severity", () => {
    // overdue (urgency 1) + importance 50 (severity 0.5)
    const s = score(item({ deadline: at(-1), importance: 50 }), DEFAULT_RANK_WEIGHT, NOW);
    expect(s).toBeCloseTo(0.6 * 1 + 0.4 * 0.5, 10); // 0.8
  });

  test("w=1 is pure urgency", () => {
    const s = score(item({ deadline: at(-1), importance: 0 }), 1, NOW);
    expect(s).toBe(1);
    const noDeadline = score(item({ deadline: null, importance: 100 }), 1, NOW);
    expect(noDeadline).toBe(0);
  });

  test("w=0 is pure severity", () => {
    const s = score(item({ deadline: at(-1), importance: 30 }), 0, NOW);
    expect(s).toBeCloseTo(0.3, 10);
  });

  test("out-of-range weight is clamped", () => {
    const hi = score(item({ deadline: at(-1), importance: 0 }), 5, NOW);
    expect(hi).toBe(1); // clamped to w=1
    const lo = score(item({ deadline: null, importance: 100 }), -5, NOW);
    expect(lo).toBe(1); // clamped to w=0 → pure severity
  });
});

describe("rankItems", () => {
  test("sorts descending by score and does not mutate input", () => {
    const items = [
      item({ deadline: null, importance: 90 }), // severity-heavy, no urgency
      item({ deadline: at(-2), importance: 10 }), // overdue
      item({ deadline: at(14), importance: 50 }), // mild
    ];
    const snapshot = [...items];
    const ranked = rankItems(items, DEFAULT_RANK_WEIGHT, NOW);
    // Overdue wins under the default (time-leaning) weight.
    expect(ranked[0]).toBe(items[1]);
    expect(items).toEqual(snapshot); // original order untouched
  });

  test("ties keep original relative order", () => {
    const a = item({ deadline: null, importance: 40 });
    const b = item({ deadline: null, importance: 40 });
    const ranked = rankItems([a, b], DEFAULT_RANK_WEIGHT, NOW);
    expect(ranked[0]).toBe(a);
    expect(ranked[1]).toBe(b);
  });

  test("weight changes the ordering (time vs severity)", () => {
    const urgent = item({ deadline: at(1), importance: 5 });
    const heavy = item({ deadline: null, importance: 95 });
    const byTime = rankItems([heavy, urgent], 1, NOW);
    expect(byTime[0]).toBe(urgent);
    const bySeverity = rankItems([urgent, heavy], 0, NOW);
    expect(bySeverity[0]).toBe(heavy);
  });

  test("empty input yields empty output", () => {
    expect(rankItems([], DEFAULT_RANK_WEIGHT, NOW)).toEqual([]);
  });
});
