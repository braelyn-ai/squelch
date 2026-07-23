// Tests for stripTrackers. Run with: `bun test`.
//
// stripTrackers is DOMParser-based, and the bun runtime ships no DOM. Unlike
// opener.test.ts (which only needs a stubbed window.open) we need a faithful,
// spec-compliant HTML parser to exercise the real tree-walking logic — so we
// register jsdom's DOMParser as the global for the duration of the suite. The
// no-DOMParser graceful-degradation path is covered by its own test that yanks
// the global back out.

import { expect, test, describe, beforeAll, afterAll } from "bun:test";
import { JSDOM } from "jsdom";
import { stripTrackers } from "./trackers";

let saved: unknown;

beforeAll(() => {
  const dom = new JSDOM();
  saved = (globalThis as { DOMParser?: unknown }).DOMParser;
  (globalThis as { DOMParser?: unknown }).DOMParser = dom.window.DOMParser;
});
afterAll(() => {
  (globalThis as { DOMParser?: unknown }).DOMParser = saved;
});

describe("stripTrackers — removes trackers", () => {
  test("strips a 1x1 attribute pixel", () => {
    const { html, blocked } = stripTrackers(
      '<p>hi</p><img src="http://track.example/p" width="1" height="1">',
    );
    expect(blocked).toBe(1);
    expect(html).not.toContain("<img");
    expect(html).toContain("<p>hi</p>");
  });

  test("strips a 2x2 attribute pixel (boundary <= 2)", () => {
    const { blocked } = stripTrackers(
      '<img src="http://track.example/p" width="2" height="2">',
    );
    expect(blocked).toBe(1);
  });

  test("strips an inline-style hidden img (display:none)", () => {
    const { html, blocked } = stripTrackers(
      '<img src="http://x.example/beacon.gif" style="display:none">',
    );
    expect(blocked).toBe(1);
    expect(html).not.toContain("<img");
  });

  test("strips an inline-style hidden img (visibility:hidden)", () => {
    const { blocked } = stripTrackers(
      '<img src="http://x.example/beacon.gif" style="visibility:hidden">',
    );
    expect(blocked).toBe(1);
  });

  test("strips an inline-style tiny img (width/height in style)", () => {
    const { blocked } = stripTrackers(
      '<img src="http://x.example/o.gif" style="width:1px;height:1px">',
    );
    expect(blocked).toBe(1);
  });

  test("strips a known tracker URL with no declared dims", () => {
    const { html, blocked } = stripTrackers(
      '<img src="https://example.list-manage.com/track/open.php?u=abc&id=xyz">',
    );
    expect(blocked).toBe(1);
    expect(html).not.toContain("<img");
  });

  test("strips a protocol-relative known tracker", () => {
    const { blocked } = stripTrackers(
      '<img src="//ct.sendgrid.net/wf/open?upn=deadbeef">',
    );
    expect(blocked).toBe(1);
  });
});

describe("stripTrackers — keeps real images (false-strip is not allowed)", () => {
  test("keeps a normal image", () => {
    const { html, blocked } = stripTrackers(
      '<img src="https://cdn.example.com/hero.jpg" width="600" height="400" alt="hero">',
    );
    expect(blocked).toBe(0);
    expect(html).toContain("hero.jpg");
  });

  test("keeps a 1x1-source img declared width=600 (layout spacer, not tiny)", () => {
    const { html, blocked } = stripTrackers(
      '<img src="https://cdn.example.com/1px.gif" width="600" height="1">',
    );
    // height=1 is tiny but width=600 is not — BOTH must be <=2 to strip.
    expect(blocked).toBe(0);
    expect(html).toContain("1px.gif");
  });

  test("keeps a width-only declared image (missing height => not tiny)", () => {
    const { blocked } = stripTrackers(
      '<img src="https://cdn.example.com/full.png" width="1">',
    );
    expect(blocked).toBe(0);
  });

  test("keeps a data: spacer with no dims and no tracker match", () => {
    const { html, blocked } = stripTrackers(
      '<img src="data:image/gif;base64,R0lGODlhAQABAAAAACwAAAAAAQABAAA=">',
    );
    expect(blocked).toBe(0);
    expect(html).toContain("data:image/gif");
  });

  test("keeps a relative-src image with no dims and no tracker match", () => {
    const { blocked } = stripTrackers('<img src="/assets/logo.png">');
    expect(blocked).toBe(0);
  });

  test("does not tracker-match a data: url that echoes a tracker string", () => {
    // A known-tracker regex must only apply to network srcs, never data:.
    const { blocked } = stripTrackers(
      '<img src="data:text/plain,pstmrk.it">',
    );
    expect(blocked).toBe(0);
  });
});

describe("stripTrackers — counting and scope", () => {
  test("blocked count reflects multiple removals", () => {
    const { html, blocked } = stripTrackers(
      [
        '<img src="https://cdn.example.com/hero.jpg" width="600" height="400">',
        '<img src="http://t/a" width="1" height="1">',
        '<img src="http://t/b" style="display:none">',
        '<img src="https://x.pstmrk.it/open">',
      ].join(""),
    );
    expect(blocked).toBe(3);
    expect(html).toContain("hero.jpg");
  });

  test("never removes non-img elements even if hidden or tiny", () => {
    const { html, blocked } = stripTrackers(
      '<div style="display:none">secret</div>' +
        '<span style="width:1px;height:1px">.</span>' +
        '<p>body</p>',
    );
    expect(blocked).toBe(0);
    expect(html).toContain("secret");
    expect(html).toContain('width:1px');
    expect(html).toContain("<p>body</p>");
  });

  test("returns the original html unchanged when nothing is stripped", () => {
    const input = '<p>hello <a href="https://x">link</a></p>';
    const { html, blocked } = stripTrackers(input);
    expect(blocked).toBe(0);
    expect(html).toBe(input);
  });
});

describe("stripTrackers — graceful degradation without DOMParser", () => {
  test("returns input unchanged with blocked:0 when DOMParser is absent", () => {
    const dp = (globalThis as { DOMParser?: unknown }).DOMParser;
    delete (globalThis as { DOMParser?: unknown }).DOMParser;
    try {
      const input = '<img src="http://t/a" width="1" height="1">';
      const { html, blocked } = stripTrackers(input);
      expect(blocked).toBe(0);
      expect(html).toBe(input);
    } finally {
      (globalThis as { DOMParser?: unknown }).DOMParser = dp;
    }
  });
});
