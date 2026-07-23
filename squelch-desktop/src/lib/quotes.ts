// Quoted reply-history detection, for auto-collapsing the tail of a message.
//
// Server-side sanitization (ammonia, defaults) strips class/id attributes, so
// the client never sees Gmail's `.gmail_quote` or Outlook's `#divRplyFwdMsg`
// markers. Detection therefore keys off what DOES survive: <blockquote>
// structure in html bodies, and ">"-prefixed lines / "On … wrote:" attribution
// lines in plain text.
//
// The bias is deliberately conservative: a wrongly-collapsed reply hides real
// content behind a click, so anything ambiguous (inline quotes with substantial
// text after them, messages that are ALL quote) is left fully expanded.

/** Attribution lines that introduce quoted history in plain text and html. */
const ATTRIBUTION_RES = [
  /^On .{1,200} wrote:\s*$/,
  /^-{2,}\s*(original|forwarded) message\s*-{0,}$/i,
  /^Begin forwarded message:?\s*$/i,
];

function isAttribution(line: string): boolean {
  const t = line.trim();
  return t.length > 0 && ATTRIBUTION_RES.some((re) => re.test(t));
}

/**
 * Split a PLAIN-TEXT body into the visible reply and its trailing quoted
 * history. Returns `quoted: null` (collapse nothing) unless the tail from the
 * first ">"/attribution line onward is genuinely history: at least two quoted
 * lines, and almost nothing that is neither quote, attribution, nor blank —
 * bottom-posted replies (real text after an inline quote) fail that check and
 * stay expanded. A message that STARTS quoted is also left alone: collapsing
 * it would blank the whole card.
 */
export function splitQuotedText(content: string): {
  visible: string;
  quoted: string | null;
} {
  const lines = content.split("\n");
  let start = -1;
  for (let i = 0; i < lines.length; i++) {
    if (/^\s*>/.test(lines[i]) || isAttribution(lines[i])) {
      start = i;
      break;
    }
  }
  if (start <= 0) return { visible: content, quoted: null };

  const tail = lines.slice(start);
  const quotedCount = tail.filter((l) => /^\s*>/.test(l)).length;
  const stray = tail.filter(
    (l) => l.trim() !== "" && !/^\s*>/.test(l) && !isAttribution(l),
  ).length;
  if (quotedCount < 2 || stray > Math.max(2, Math.floor(tail.length * 0.2))) {
    return { visible: content, quoted: null };
  }
  return {
    visible: lines.slice(0, start).join("\n").trimEnd(),
    quoted: tail.join("\n"),
  };
}

/** Rough visible-text length of everything in `root` positioned AFTER `node`
 *  (document order), excluding `node`'s own subtree. */
function textLengthAfter(root: HTMLElement, node: HTMLElement): number {
  let len = 0;
  const walker = root.ownerDocument.createTreeWalker(root, 4 /* TEXT */);
  let t: Node | null;
  while ((t = walker.nextNode())) {
    if (node.contains(t)) continue;
    if (node.compareDocumentPosition(t) & 4 /* FOLLOWING */) {
      len += (t.textContent ?? "").trim().length;
    }
  }
  return len;
}

/**
 * Find the elements of an HTML body that make up trailing quoted history, for
 * the parent to hide (the mail iframe can never run script, so collapsing is
 * done from outside — see EmailFrame). Returns [] when nothing safely
 * collapsible is found.
 *
 * Heuristic: the first TOP-LEVEL <blockquote> after which the document has no
 * substantial text of its own anchors the history; it, every following
 * blockquote, and an immediately-preceding "On … wrote:" attribution element
 * are all collapsed together. A blockquote with real reply text after it
 * (bottom-posting, quoted-inline styles) never qualifies.
 */
export function findQuoteNodes(doc: Document): HTMLElement[] {
  const body = doc.body;
  if (!body) return [];
  const quotes = Array.from(body.querySelectorAll("blockquote")).filter(
    (b) => !b.parentElement?.closest("blockquote"),
  );
  for (const q of quotes) {
    if (textLengthAfter(body, q) > 200) continue;
    const nodes: HTMLElement[] = [];
    // The attribution line ("On … wrote:") usually sits just above the quote,
    // either as the previous sibling or as the previous sibling of a wrapper.
    // No `instanceof HTMLElement` here: the test runtime (bun + jsdom docs)
    // has no such global. An element node from this document is close enough.
    const prev =
      q.previousElementSibling ?? q.parentElement?.previousElementSibling;
    if (
      prev &&
      (prev.textContent ?? "").trim().length < 300 &&
      isAttribution((prev.textContent ?? "").replace(/\s+/g, " "))
    ) {
      nodes.push(prev as HTMLElement);
    }
    nodes.push(q);
    for (const r of quotes) {
      if (r !== q && q.compareDocumentPosition(r) & 4 /* FOLLOWING */) {
        nodes.push(r);
      }
    }
    return nodes;
  }
  return [];
}
