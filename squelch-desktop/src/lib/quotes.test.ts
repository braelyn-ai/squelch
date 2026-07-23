// Tests for quoted reply-history detection. Run with: `bun test`.
//
// splitQuotedText is pure string work; findQuoteNodes needs a real DOM, so we
// parse fixtures with jsdom's DOMParser (same approach as trackers.test.ts).

import { expect, test, describe, beforeAll, afterAll } from "bun:test";
import { JSDOM } from "jsdom";
import { splitQuotedText, findQuoteNodes } from "./quotes";

describe("splitQuotedText", () => {
  test("collapses a gmail-style trailing quote with attribution", () => {
    const { visible, quoted } = splitQuotedText(
      "Sounds good, see you then.\n\n" +
        "On Mon, Jul 20, 2026 at 9:14 AM Ana <ana@example.com> wrote:\n" +
        "> are we still on for tuesday?\n" +
        "> the 3pm slot works best for me\n",
    );
    expect(visible).toBe("Sounds good, see you then.");
    expect(quoted).toContain("are we still on");
  });

  test("collapses bare >-quoted tail without attribution", () => {
    const { visible, quoted } = splitQuotedText(
      "Done!\n\n> please run the migration\n> before friday\n> thanks\n",
    );
    expect(visible).toBe("Done!");
    expect(quoted).not.toBeNull();
  });

  test("leaves bottom-posted replies alone (text after the quote)", () => {
    const { quoted } = splitQuotedText(
      "> should we ship on friday?\n\n" +
        "I think we should wait for the QA pass to finish first,\n" +
        "and give ops a heads-up before anything goes out.\n" +
        "Friday morning at the earliest.\n",
    );
    expect(quoted).toBeNull();
  });

  test("leaves a message that is entirely quoted alone", () => {
    const all = "> line one\n> line two\n> line three";
    const { visible, quoted } = splitQuotedText(all);
    expect(quoted).toBeNull();
    expect(visible).toBe(all);
  });

  test("a single stray quoted line does not trigger collapse", () => {
    const { quoted } = splitQuotedText("As I said:\n> the one liner\nmore prose after");
    expect(quoted).toBeNull();
  });

  test("no quotes at all — untouched", () => {
    const body = "Just a normal email.\nTwo lines.";
    expect(splitQuotedText(body)).toEqual({ visible: body, quoted: null });
  });
});

describe("findQuoteNodes", () => {
  let saved: unknown;
  beforeAll(() => {
    const dom = new JSDOM();
    saved = (globalThis as { DOMParser?: unknown }).DOMParser;
    (globalThis as { DOMParser?: unknown }).DOMParser = dom.window.DOMParser;
  });
  afterAll(() => {
    (globalThis as { DOMParser?: unknown }).DOMParser = saved;
  });

  const parse = (html: string) =>
    new DOMParser().parseFromString(html, "text/html");

  test("collapses a trailing blockquote plus its attribution line", () => {
    const doc = parse(
      "<p>Sounds good!</p>" +
        "<div>On Mon, Jul 20, 2026 at 9:14 AM Ana wrote:</div>" +
        "<blockquote><p>are we still on for tuesday?</p></blockquote>",
    );
    const nodes = findQuoteNodes(doc);
    expect(nodes.length).toBe(2);
    expect(nodes[0].textContent).toContain("wrote:");
    expect(nodes[1].tagName).toBe("BLOCKQUOTE");
  });

  test("collapses all trailing blockquotes of a long chain", () => {
    const doc = parse(
      "<p>ok</p>" +
        "<blockquote>reply 2<blockquote>reply 1</blockquote></blockquote>" +
        "<blockquote>stray sibling quote</blockquote>",
    );
    const nodes = findQuoteNodes(doc);
    // Two TOP-LEVEL blockquotes (the nested one rides along inside the first).
    expect(nodes.length).toBe(2);
  });

  test("a blockquote followed by substantial reply text is not collapsed", () => {
    const doc = parse(
      "<blockquote>should we ship friday?</blockquote>" +
        `<p>${"I think we should wait for the QA pass to finish first. ".repeat(6)}</p>`,
    );
    expect(findQuoteNodes(doc)).toEqual([]);
  });

  test("no blockquotes — nothing to collapse", () => {
    expect(findQuoteNodes(parse("<p>plain marketing mail</p>"))).toEqual([]);
  });
});
