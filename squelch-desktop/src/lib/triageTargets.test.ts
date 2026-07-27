// Tests for the triage-fix target matcher. Run with: `bun test`.
//
// Pure string ranking, no DOM. The properties that matter here are about NOT
// guessing: an ambiguous prefix must surface every candidate rather than
// silently resolving to one, because whichever one wins gets written into the
// training set as ground truth.

import { expect, test, describe } from "bun:test";
import {
  TRIAGE_TARGETS,
  matchTargets,
  scoreTarget,
  targetLabel,
} from "./triageTargets";

describe("matchTargets", () => {
  test("the motivating case: 'bill' finds the billing categories first", () => {
    const hits = matchTargets("bill");
    expect(hits.length).toBeGreaterThan(0);
    // Both bill-ish categories must be present — "bill" genuinely is ambiguous
    // between them, and picking one silently would write a wrong label.
    const values = hits.map((h) => h.value);
    expect(values).toContain("invoice");
    expect(values).toContain("autopay_bill");
    // And the top hit is a category, not a tier.
    expect(hits[0].axis).toBe("category");
  });

  test("an exact value outranks an alias match", () => {
    const hits = matchTargets("invoice");
    expect(hits[0].value).toBe("invoice");
  });

  test("a prefix of the value wins", () => {
    expect(matchTargets("noi")[0].value).toBe("noise");
    expect(matchTargets("past")[0].value).toBe("past_due");
  });

  test("underscores, spaces and case are all the same thing", () => {
    for (const q of ["past_due", "PAST DUE", "pastdue", "Past-Due"]) {
      expect(matchTargets(q)[0].value).toBe("past_due");
    }
  });

  test("an empty query shows everything, in declaration order", () => {
    const hits = matchTargets("");
    expect(hits.length).toBe(TRIAGE_TARGETS.length);
    expect(hits[0].value).toBe(TRIAGE_TARGETS[0].value);
  });

  test("nonsense matches nothing rather than guessing", () => {
    expect(matchTargets("zzzzq")).toEqual([]);
  });

  test("tiers are reachable by their own words", () => {
    expect(matchTargets("junk")[0].value).toBe("noise");
    expect(matchTargets("overdue")[0].value).toBe("past_due");
    expect(matchTargets("signal")[0].value).toBe("signal");
  });
});

describe("scoreTarget", () => {
  test("exact beats prefix beats alias beats substring", () => {
    const invoice = TRIAGE_TARGETS.find((t) => t.value === "invoice")!;
    expect(scoreTarget(invoice, "invoice")).toBeGreaterThan(
      scoreTarget(invoice, "invo"),
    );
    expect(scoreTarget(invoice, "invo")).toBeGreaterThan(
      scoreTarget(invoice, "bill"),
    );
    expect(scoreTarget(invoice, "bill")).toBeGreaterThan(
      scoreTarget(invoice, "voic"),
    );
  });
});

describe("targetLabel", () => {
  test("renders wire values as the labels humans saw", () => {
    expect(targetLabel("category", "autopay_bill")).toBe("Autopay bill");
    expect(targetLabel("tier", "past_due")).toBe("Past due");
  });

  test("an unset or unknown value degrades readably", () => {
    expect(targetLabel("category", null)).toBe("unset");
    // A value from a newer pipeline shows through rather than vanishing.
    expect(targetLabel("category", "brand_new_thing")).toBe("brand_new_thing");
  });
});
