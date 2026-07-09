// Pure-logic tests for the newsletters derivation. Run with: `bun test`.
// Calibrated against squelch-core rung-5 reason strings.

import { expect, test, describe } from "bun:test";
import {
  deriveNewsletters,
  ruleMatchesAddress,
  ruleForAddress,
  senderAddress,
  domainPattern,
} from "./newsletters";
import type { AttentionUpdate, SenderRule } from "../api";

const NOW = Date.now();

function upd(p: Partial<AttentionUpdate> & { sender: string; reason: string }): AttentionUpdate {
  return {
    id: Math.floor(Math.random() * 1e9),
    thread_id: "t",
    tier: "noise",
    importance: 10,
    sender: p.sender,
    one_line: p.one_line ?? "some digest line",
    reason: p.reason,
    deadline: null,
    matched_rule: null,
    status: "new",
    surfaced_at: p.surfaced_at ?? new Date(NOW).toISOString(),
    resolved_at: null,
    ...p,
  };
}

function rule(pattern: string, extra?: Partial<SenderRule>): SenderRule {
  return {
    id: 1,
    account_id: 1,
    match_pattern: pattern,
    want_text: "",
    disposition: "filtered",
    updated_at: new Date(NOW).toISOString(),
    ...extra,
  };
}

describe("ruleMatchesAddress", () => {
  test("*@domain matches any local part", () => {
    expect(ruleMatchesAddress("*@acme.com", "news@acme.com")).toBe(true);
    expect(ruleMatchesAddress("*@acme.com", "billing@acme.com")).toBe(true);
    expect(ruleMatchesAddress("*@acme.com", "x@other.com")).toBe(false);
  });
  test("exact address matches only itself; case-insensitive", () => {
    expect(ruleMatchesAddress("Billing@Acme.com", "billing@acme.com")).toBe(true);
    expect(ruleMatchesAddress("billing@acme.com", "news@acme.com")).toBe(false);
  });
});

describe("ruleForAddress", () => {
  test("prefers the most specific (fewest wildcards) match", () => {
    const rules = [rule("*@acme.com", { id: 1 }), rule("news@acme.com", { id: 2 })];
    expect(ruleForAddress(rules, "news@acme.com")?.id).toBe(2);
  });
  test("null when no rule governs the address", () => {
    expect(ruleForAddress([rule("*@acme.com")], "x@other.com")).toBeNull();
  });
});

describe("deriveNewsletters", () => {
  test("includes an unsubscribe-footer sender (the newsletter reason)", () => {
    const nls = deriveNewsletters(
      [upd({ sender: "News <news@acme.com>", reason: "bulk/list mail (unsubscribe footer)" })],
      [],
    );
    expect(nls.length).toBe(1);
    expect(nls[0].address).toBe("news@acme.com");
    expect(nls[0].count).toBe(1);
  });

  test("excludes a receipt-only sender (order confirmation / receipt)", () => {
    const nls = deriveNewsletters(
      [
        upd({ sender: "orders@shop.com", reason: "order confirmation / receipt" }),
        upd({ sender: "orders@shop.com", reason: "order confirmation / receipt" }),
      ],
      [],
    );
    expect(nls.length).toBe(0);
  });

  test("admits a recurring robot sender with >=2 noise messages, no newsletter reason", () => {
    const nls = deriveNewsletters(
      [
        upd({ sender: "no-reply@brandy.com", reason: "matched squelch rule #3 (mute)" }),
        upd({ sender: "no-reply@brandy.com", reason: "matched squelch rule #3 (mute)" }),
      ],
      [],
    );
    expect(nls.length).toBe(1);
    expect(nls[0].count).toBe(2);
  });

  test("a single robot message with no newsletter reason does NOT qualify", () => {
    const nls = deriveNewsletters(
      [upd({ sender: "no-reply@brandy.com", reason: "no Stage-1 rule matched" })],
      [],
    );
    expect(nls.length).toBe(0);
  });

  test("a newsletter sender that also has a receipt still qualifies (mixed)", () => {
    const nls = deriveNewsletters(
      [
        upd({ sender: "news@acme.com", reason: "bulk/list mail (unsubscribe footer)" }),
        upd({ sender: "news@acme.com", reason: "order confirmation / receipt" }),
      ],
      [],
    );
    expect(nls.length).toBe(1);
    expect(nls[0].count).toBe(2);
  });

  test("attaches a matching rule to the card", () => {
    const nls = deriveNewsletters(
      [upd({ sender: "news@acme.com", reason: "bulk/list mail (unsubscribe footer)" })],
      [rule("*@acme.com", { disposition: "filtered", want_text: "only sales" })],
    );
    expect(nls[0].rule?.disposition).toBe("filtered");
  });

  test("drops messages older than the 7-day window", () => {
    const old = new Date(NOW - 10 * 86_400_000).toISOString();
    const nls = deriveNewsletters(
      [upd({ sender: "news@acme.com", reason: "bulk/list mail (unsubscribe footer)", surfaced_at: old })],
      [],
    );
    expect(nls.length).toBe(0);
  });

  test("groups by sender and picks the latest one_line as the summary", () => {
    const older = new Date(NOW - 3_600_000).toISOString();
    const nls = deriveNewsletters(
      [
        upd({ sender: "news@acme.com", reason: "bulk/list mail (unsubscribe footer)", one_line: "old", surfaced_at: older }),
        upd({ sender: "news@acme.com", reason: "bulk/list mail (unsubscribe footer)", one_line: "newest" }),
      ],
      [],
    );
    expect(nls[0].count).toBe(2);
    expect(nls[0].summary).toBe("newest");
  });
});

describe("helpers", () => {
  test("senderAddress extracts + lowercases the bare address", () => {
    expect(senderAddress("Sarah Chen <Sarah@Acme.com>")).toBe("sarah@acme.com");
    expect(senderAddress("news@acme.com")).toBe("news@acme.com");
  });
  test("domainPattern builds *@domain, collapsing mail subdomains", () => {
    expect(domainPattern("news@acme.com")).toBe("*@acme.com");
    expect(domainPattern("x@mail.acme.com")).toBe("*@acme.com");
  });
});
