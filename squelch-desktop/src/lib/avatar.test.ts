// Sender-avatar classification + the favicon verdict cache.
//
// Both of these failed silently in the field, which is why they now have tests:
//   - robot detection matched only the WHOLE local-part, so obvious machines
//     ("no.reply.alerts@chase.com") fell through to initials;
//   - a failed verdict was stored forever, so one transient network blip
//     permanently disabled a domain's icon.

import { describe, expect, test, beforeEach } from "bun:test";
import {
  isRobotSender,
  isBrandSender,
  faviconDomain,
  initialsFor,
  senderDisplayName,
  faviconVerdict,
  setFaviconVerdict,
} from "./avatar";

/** Is this sender eligible for a network favicon at all? */
const eligible = (s: string) =>
  (isRobotSender(s) || isBrandSender(s)) && faviconDomain(s) !== null;

describe("robot detection", () => {
  test("plain robot mailboxes match", () => {
    for (const s of [
      "no-reply@github.com",
      "noreply@github.com",
      "notifications@github.com",
      "billing@stripe.com",
      "service@paypal.com",
      "donotreply@usps.com",
    ]) {
      expect(isRobotSender(s)).toBe(true);
    }
  });

  test("separator-split robots match — the regression that showed initials", () => {
    // Every one of these is unmistakably a machine, and every one of them used
    // to fail because the pattern had to match the entire local-part.
    for (const s of [
      "no.reply.alerts@chase.com",
      "no_reply@discord.com",
      "billing-noreply@stripe.com",
      "no-reply-aws@amazon.com",
      "do.not.reply@united.com",
    ]) {
      expect(isRobotSender(s)).toBe(true);
    }
  });

  test("bulk-mail local-parts match", () => {
    expect(isRobotSender("email@e.ebay.com")).toBe(true);
    expect(isRobotSender("statements@schwab.com")).toBe(true);
  });

  test("humans NEVER match — a false positive is a privacy leak", () => {
    for (const s of [
      "sarah@acme.com",
      "bboynton97@gmail.com",
      "rebelneonstudios@gmail.com",
      "j.smith@lawfirm.com",
      // Human-capable words stay WHOLE-local-part matches only, so a person
      // whose address merely contains one is not resolved over the network.
      "jane.support@acme.com",
      "dave.team@startup.io",
      "chris.info@agency.co",
    ]) {
      expect(isRobotSender(s)).toBe(false);
    }
  });
});

describe("favicon domain", () => {
  test("peels one mail-ish subdomain, two-label minimum", () => {
    expect(faviconDomain("news@e.chase.com")).toBe("chase.com");
    expect(faviconDomain("x@notifications.github.com")).toBe("github.com");
    expect(faviconDomain("x@example.com")).toBe("example.com");
    expect(faviconDomain("x@localhost")).toBeNull();
  });

  test("brand mailboxes are eligible", () => {
    expect(isBrandSender("eBay@eBay.com")).toBe(true);
    expect(eligible("eBay@eBay.com")).toBe(true);
  });
});

describe("display name", () => {
  test("prefers a display name, then brand, then robot base domain", () => {
    expect(senderDisplayName("Sarah Chen <sarah@acme.com>")).toBe("Sarah Chen");
    expect(senderDisplayName("eBay@eBay.com")).toBe("eBay");
    expect(senderDisplayName("no-reply@stripe.com")).toBe("Stripe");
    expect(senderDisplayName("sarah@acme.com")).toBe("sarah@acme.com");
  });
});

describe("initials", () => {
  test("a display name gives word initials", () => {
    expect(initialsFor("Sarah Chen <sarah@acme.com>")).toBe("SC");
    expect(initialsFor("Corpnet <x@corpnet.com>")).toBe("CO");
  });

  test("the DOMAIN never contributes an initial", () => {
    // The old behaviour split the whole address, so ".com" supplied the second
    // letter and nearly every bare address rendered "?C".
    expect(initialsFor("bboynton97@gmail.com")).toBe("BB");
    expect(initialsFor("rebelneonstudios@gmail.com")).toBe("RE");
    expect(initialsFor("q@acme.com")).toBe("Q");
  });

  test("initials agree with the name actually displayed", () => {
    // The row shows "Corpnet", so the monogram is CO — not the IC that the
    // raw local-part "info" would have produced.
    expect(senderDisplayName("info@corpnet.com")).toBe("Corpnet");
    expect(initialsFor("info@corpnet.com")).toBe("CO");
  });
});

describe("favicon verdict cache", () => {
  const DAY = 24 * 60 * 60 * 1000;
  let n = 0;
  // A distinct domain per test so the module-level cache cannot leak between
  // them (the store is process-global by design).
  beforeEach(() => {
    n += 1;
  });
  const dom = () => `t${n}.example.com`;

  test("an unknown domain is unresolved", () => {
    expect(faviconVerdict(dom())).toBeNull();
  });

  test("ok is permanent", () => {
    const d = dom();
    const t0 = 1_000_000;
    setFaviconVerdict(d, "ok", t0);
    expect(faviconVerdict(d, t0)).toBe("ok");
    expect(faviconVerdict(d, t0 + 3650 * DAY)).toBe("ok");
  });

  test("a failure is trusted for a week, then retried", () => {
    const d = dom();
    const t0 = 1_000_000;
    setFaviconVerdict(d, "failed", t0);
    expect(faviconVerdict(d, t0)).toBe("failed");
    expect(faviconVerdict(d, t0 + 6 * DAY)).toBe("failed");
    // THE FIX: a stale failure reads as unresolved, so the domain is retried
    // instead of being written off forever.
    expect(faviconVerdict(d, t0 + 8 * DAY)).toBeNull();
  });

  test("a retry that succeeds sticks", () => {
    const d = dom();
    const t0 = 1_000_000;
    setFaviconVerdict(d, "failed", t0);
    setFaviconVerdict(d, "ok", t0 + 8 * DAY);
    expect(faviconVerdict(d, t0 + 9 * DAY)).toBe("ok");
  });
});
