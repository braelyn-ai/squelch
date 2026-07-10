// Guard tests for openExternal. Run with: `bun test`.
// The bun runtime has no DOM, so we install a minimal `window` (no
// __TAURI_INTERNALS__) — openExternal then takes the window.open branch, which
// we stub to assert the http/https scheme guard. Matches the repo's no-jsdom,
// pure-logic testing convention.

import { expect, test, describe, beforeEach, afterEach, mock } from "bun:test";
import { openExternal } from "./opener";

describe("openExternal", () => {
  let opened: string[];

  beforeEach(() => {
    opened = [];
    (globalThis as { window?: unknown }).window = {
      open: mock((url: string) => {
        opened.push(url);
        return null;
      }),
    };
  });
  afterEach(() => {
    delete (globalThis as { window?: unknown }).window;
  });

  test("opens https urls", async () => {
    await openExternal("https://ups.com/track?n=1");
    expect(opened).toEqual(["https://ups.com/track?n=1"]);
  });

  test("opens http urls", async () => {
    await openExternal("http://example.com/");
    expect(opened).toEqual(["http://example.com/"]);
  });

  test("ignores non-http schemes (mailto/javascript/data/file)", async () => {
    await openExternal("mailto:a@b.com");
    await openExternal("javascript:alert(1)");
    await openExternal("data:text/html,<b>x</b>");
    await openExternal("file:///etc/passwd");
    expect(opened).toEqual([]);
  });

  test("ignores empty/garbage input", async () => {
    await openExternal("");
    await openExternal("not a url");
    expect(opened).toEqual([]);
  });
});
