// Tests for the email image cache. Run with: `bun test`.
//
// Two things need faking. (1) A DOM: collection and inlining are DOMParser-based
// and the bun runtime ships no DOM, so — as in trackers.test.ts — jsdom's
// DOMParser is registered as the global for the suite. (2) The Tauri shell:
// `@tauri-apps/api/core` is mocked so `isTauri()` reports true and `invoke`
// returns a deterministic data URL instead of hitting the network. The mock has
// to be installed BEFORE the module under test is loaded, hence the dynamic
// import below.

import { expect, test, describe, beforeAll, afterAll, beforeEach, mock } from "bun:test";
import { JSDOM } from "jsdom";

/** URLs the fake shell was asked to fetch, in call order. */
let fetched: string[] = [];
/** URLs the fake shell should fail (missing image / bad host). */
let failing = new Set<string>();

mock.module("@tauri-apps/api/core", () => ({
  isTauri: () => true,
  invoke: async (cmd: string, args: { url: string }) => {
    if (cmd !== "fetch_image_data_url") throw new Error(`unexpected command ${cmd}`);
    fetched.push(args.url);
    if (failing.has(args.url)) throw new Error("image fetch failed");
    // Deterministic and URL-derived, so assertions can tie a rendered data URL
    // back to the request that produced it.
    return `data:image/png;base64,${btoa(args.url)}`;
  },
}));

const {
  collectImageUrls,
  normalizeImageUrl,
  inlineImages,
  warmImageCache,
  imagesSettled,
  hasNetworkImages,
  __resetImageCache,
} = await import("./imageCache");

/** The data URL the fake shell produces for a given source URL. */
const dataFor = (url: string) => `data:image/png;base64,${btoa(url)}`;

const originalDOMParser = globalThis.DOMParser;

beforeAll(() => {
  globalThis.DOMParser = new JSDOM().window.DOMParser;
});
afterAll(() => {
  globalThis.DOMParser = originalDOMParser;
});
beforeEach(() => {
  __resetImageCache();
  fetched = [];
  failing = new Set();
});

describe("normalizeImageUrl", () => {
  test("protocol-relative refs resolve to https", () => {
    expect(normalizeImageUrl("//cdn.x.com/a.png")).toBe("https://cdn.x.com/a.png");
  });

  test("http and https pass through, trimmed", () => {
    expect(normalizeImageUrl("  http://x.com/a.png ")).toBe("http://x.com/a.png");
    expect(normalizeImageUrl("https://x.com/a.png")).toBe("https://x.com/a.png");
  });

  test("non-network refs are not ours to touch", () => {
    // data: is already inline, cid: is an unresolvable attachment ref, and a
    // relative path has no meaning under the srcdoc origin.
    expect(normalizeImageUrl("data:image/png;base64,AAAA")).toBeNull();
    expect(normalizeImageUrl("cid:part1@mail")).toBeNull();
    expect(normalizeImageUrl("/images/a.png")).toBeNull();
    expect(normalizeImageUrl("")).toBeNull();
  });
});

describe("collectImageUrls", () => {
  test("finds img src", () => {
    const html = '<img src="https://x.com/hero.png">';
    expect(collectImageUrls(html)).toEqual(["https://x.com/hero.png"]);
  });

  test("finds CSS url() in inline styles — the class the old warmer missed", () => {
    // Ammonia strips <td background> but KEEPS inline style attributes, so this
    // is how real marketing mail ships its background art. NOTE the wrapping
    // table: a bare <td> is discarded outright by the HTML parser, so fixtures
    // have to carry the same structure real sanitized mail does.
    const html =
      '<table><tr><td style="background-image:url(https://x.com/bg.jpg);color:red">hi</td></tr></table>';
    expect(collectImageUrls(html)).toEqual(["https://x.com/bg.jpg"]);
  });

  test("handles quoted, unquoted and whitespaced url() forms", () => {
    const html =
      '<div style="background:url(\'https://x.com/a.png\')"></div>' +
      '<div style="background:url( https://x.com/b.png )"></div>' +
      '<div style="background:url(&quot;https://x.com/c.png&quot;)"></div>';
    expect(collectImageUrls(html)).toEqual([
      "https://x.com/a.png",
      "https://x.com/b.png",
      "https://x.com/c.png",
    ]);
  });

  test("covers background attributes and srcset candidates", () => {
    const html =
      '<table><tr><td background="//x.com/bg.gif"></td></tr></table>' +
      '<img src="https://x.com/1.png" srcset="https://x.com/2.png 2x, https://x.com/3.png 3x">';
    expect(collectImageUrls(html)).toEqual([
      "https://x.com/bg.gif",
      "https://x.com/1.png",
      "https://x.com/2.png",
      "https://x.com/3.png",
    ]);
  });

  test("de-dupes by normalized url, keeping document order", () => {
    const html =
      '<img src="https://x.com/a.png">' +
      '<div style="background:url(//x.com/b.png)"></div>' +
      '<img src="//x.com/a.png">';
    expect(collectImageUrls(html)).toEqual([
      "https://x.com/a.png",
      "https://x.com/b.png",
    ]);
  });

  test("ignores non-network refs", () => {
    const html =
      '<img src="data:image/png;base64,AAAA"><img src="cid:x@y"><img src="/rel.png">';
    expect(collectImageUrls(html)).toEqual([]);
  });

  test("a pathological document is capped rather than enqueuing thousands", () => {
    const html = Array.from(
      { length: 400 },
      (_, i) => `<img src="https://x.com/${i}.png">`,
    ).join("");
    // Cap is deliberately far above any real email; assert it holds and that
    // it is nowhere near the 12 that caused the flicker.
    expect(collectImageUrls(html).length).toBe(150);
  });
});

describe("hasNetworkImages", () => {
  test("true for CSS-background-only mail (no <img> at all)", () => {
    // This mail would previously have shown NO "load images" opt-in bar,
    // because that check only looked for <img src>.
    expect(
      hasNetworkImages(
        '<table><tr><td style="background:url(https://x.com/a.png)"></td></tr></table>',
      ),
    ).toBe(true);
  });

  test("false when nothing points at the network", () => {
    expect(hasNetworkImages("<p>plain text</p>")).toBe(false);
    expect(hasNetworkImages('<img src="cid:logo@mail">')).toBe(false);
  });
});

describe("warmImageCache + inlineImages", () => {
  test("warms every reference, then renders with zero network refs", async () => {
    const html =
      '<img src="https://x.com/hero.png">' +
      '<table><tr><td style="background-image:url(https://x.com/bg.jpg)"></td></tr></table>';
    await warmImageCache(html);
    expect(fetched.sort()).toEqual([
      "https://x.com/bg.jpg",
      "https://x.com/hero.png",
    ]);

    const r = inlineImages(html);
    expect(r.inlined).toBe(2);
    expect(r.remote).toBe(0); // => the frame can drop to `img-src data:`
    expect(r.html).not.toContain("https://x.com/");

    // Assert against the PARSED result, not the serialized string: the quotes
    // we write into the style attribute come back out as &quot; entities, and
    // what matters is the value the browser's parser hands to the CSS engine.
    const doc = new DOMParser().parseFromString(r.html, "text/html");
    expect(doc.querySelector("img")?.getAttribute("src")).toBe(
      dataFor("https://x.com/hero.png"),
    );
    expect(doc.querySelector("td")?.getAttribute("style")).toBe(
      `background-image:url("${dataFor("https://x.com/bg.jpg")}")`,
    );
  });

  test("inlining an img drops its srcset so the src cannot be outranked", async () => {
    const html =
      '<img src="https://x.com/a.png" srcset="https://x.com/a2.png 2x">';
    await warmImageCache(html);
    const r = inlineImages(html);
    expect(r.html).not.toContain("srcset");
    expect(r.remote).toBe(0);
  });

  test("a srcset that survives keeps the frame's remote grant", () => {
    // Nothing warmed: src stays remote, so the srcset does too. Both must be
    // reflected in `remote` or the CSP would tighten and break the image.
    const html = '<img src="https://x.com/a.png" srcset="https://x.com/a2.png 2x">';
    const r = inlineImages(html);
    expect(r.inlined).toBe(0);
    expect(r.remote).toBeGreaterThan(0);
  });

  test("uncached references are left exactly as they were", async () => {
    const html =
      '<img src="https://x.com/warm.png"><img src="https://x.com/cold.png">';
    // Warm only the first by handing warmImageCache a one-image document.
    await warmImageCache('<img src="https://x.com/warm.png">');

    const r = inlineImages(html);
    expect(r.inlined).toBe(1);
    expect(r.remote).toBe(1); // still needs the network grant
    expect(r.html).toContain(dataFor("https://x.com/warm.png"));
    expect(r.html).toContain('src="https://x.com/cold.png"');
  });

  test("non-network refs are untouched and never counted as remote", async () => {
    const html = '<img src="cid:logo@mail"><img src="https://x.com/a.png">';
    await warmImageCache(html);
    const r = inlineImages(html);
    expect(r.inlined).toBe(1);
    expect(r.remote).toBe(0);
    expect(r.html).toContain('src="cid:logo@mail"');
  });

  test("a failed fetch degrades to the remote url rather than a broken image", async () => {
    failing.add("https://x.com/gone.png");
    const html = '<img src="https://x.com/gone.png">';
    await warmImageCache(html);
    const r = inlineImages(html);
    expect(r.inlined).toBe(0);
    expect(r.remote).toBe(1);
    expect(r.html).toContain('src="https://x.com/gone.png"');
  });

  test("concurrent callers for one url share a single shell fetch", async () => {
    const html = '<img src="https://x.com/same.png">';
    await Promise.all([warmImageCache(html), warmImageCache(html), warmImageCache(html)]);
    expect(fetched).toEqual(["https://x.com/same.png"]);
  });

  test("re-warming a warm message costs no further fetches", async () => {
    const html = '<img src="https://x.com/a.png">';
    await warmImageCache(html);
    await warmImageCache(html);
    expect(fetched.length).toBe(1);
  });
});

describe("imagesSettled", () => {
  test("false while a reference is still outstanding, true once warmed", async () => {
    const html = '<img src="https://x.com/a.png">';
    expect(imagesSettled(html)).toBe(false);
    await warmImageCache(html);
    expect(imagesSettled(html)).toBe(true);
  });

  test("a known failure counts as settled — waiting longer buys nothing", async () => {
    failing.add("https://x.com/gone.png");
    const html = '<img src="https://x.com/gone.png">';
    await warmImageCache(html);
    expect(imagesSettled(html)).toBe(true);
  });

  test("mail with no network images is settled immediately", () => {
    expect(imagesSettled("<p>hi</p>")).toBe(true);
  });
});
