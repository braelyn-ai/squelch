// Byte-level cache of remote email images, fetched through the Tauri shell and
// held as `data:` URIs so an email frame can render with ZERO network fetches.
//
// WHY THIS EXISTS (the flicker):
// The frame is a srcdoc iframe built once per open. Scripts can never run
// inside it, so the ONLY thing that changes its content height after load is
// images arriving — every late image fires a re-measure and visibly jumps the
// frame. Marketing mail is the worst case: dozens of images, most below the
// fold. The previous warmer (`new Image()` into the shared WKWebView HTTP
// cache) narrowed that window but could not close it, because:
//   * it warmed only the first 12 <img src> per message, in document order —
//     so it warmed the top of the mail and abandoned exactly the below-the-fold
//     images whose late arrival does the reflowing;
//   * it never saw CSS `url()` at all. Ammonia strips <style> blocks and the
//     <td background> attribute server-side, but it deliberately KEEPS inline
//     `style` attributes (sync/html.rs, deviation 3) — so
//     `style="background-image:url(…)"` survives, loads over the network, and
//     was 100% uncovered;
//   * it assumed one `Image()` meant "cached forever". A `no-store` response or
//     a webview eviction silently un-cached it, and the warmed-URL memo made
//     sure we never tried again.
// Above all it was a HOPE: nothing pinned the bytes. This module pins them.
// Once every reference in a message resolves to a data: URI, the frame makes no
// requests, so there is nothing left to arrive late and nothing to reflow.
//
// PRIVACY: fetching happens in Rust (no cookies, no Referer — matching the
// frame's referrerPolicy) and is gated by the same `loadRemoteImages` pref as
// every other remote-image path. When that pref is off we fetch NOTHING; the
// frame's `img-src data:` CSP stays the boundary and inlining is not attempted,
// because inlining would otherwise smuggle a network fetch past a CSP the user
// turned on precisely to prevent one. Tracking pixels are stripped upstream
// (lib/trackers) before any html reaches this module.
//
// DEGRADATION: on a stale shell binary (the command ships with the Rust side —
// a full app restart is required) or in browser dev, every fetch fails, the
// module marks itself unavailable, and callers fall back to today's behavior:
// remote URLs, permissive CSP, and the old flicker. Nothing breaks.

import { invoke, isTauri } from "@tauri-apps/api/core";

/** Concurrent shell fetches. The Rust side pools connections across all of
 *  them, so this bounds work-in-flight, not sockets-per-host. */
const MAX_CONCURRENT = 6;
/** Total cache budget, in data-URI string length (~4/3 of the raw bytes).
 *  LRU-evicted; sized to hold a warmed inbox of image-heavy newsletters. */
const MAX_CACHE_CHARS = 48 * 1024 * 1024;
/** One image may not exceed this share of the budget — a single megaimage must
 *  not be able to evict everything else. (Rust caps the raw fetch at 8MB.) */
const MAX_IMAGE_CHARS = 4 * 1024 * 1024;
/** A failed URL is not retried for this long (bad hosts are usually still bad
 *  a minute later, and a warm sweep re-requests the same set constantly). */
const FAILURE_TTL_MS = 5 * 60_000;
/** Stampede guard: images collected per message. NOT a coverage limit like the
 *  12 that caused the flicker — it sits far above any real email (the p99
 *  marketing mail is well under 60) and only exists so a pathological document
 *  can't enqueue thousands of fetches. The real limiter is MAX_CACHE_CHARS. */
const MAX_PER_MESSAGE = 150;

/** url -> data URI. Insertion order IS the LRU order (re-inserted on hit). */
const cache = new Map<string, string>();
let cacheChars = 0;
/** url -> ms timestamp of last failure (negative cache, see FAILURE_TTL_MS). */
const failures = new Map<string, number>();
/** In-flight (INCLUDING still-queued) fetches, so N callers share one request. */
const inflight = new Map<string, Promise<string | null>>();

interface Job {
  url: string;
  /** Urgent jobs (an email being opened right now) jump ahead of background
   *  inbox warming. A queued background job is promoted in place on demand. */
  urgent: boolean;
  start: () => void;
}
const queue: Job[] = [];
let active = 0;

/** Flips to true when the shell command is missing (stale binary / browser
 *  dev): every later call short-circuits instead of hammering a dead IPC. */
let unavailable = !isTauriSafe();
let warned = false;

function isTauriSafe(): boolean {
  try {
    return isTauri();
  } catch {
    return false;
  }
}

/** True when shell image fetching can be attempted at all. */
export function imageCacheAvailable(): boolean {
  return !unavailable;
}

function markUnavailable(reason: unknown): void {
  unavailable = true;
  if (warned) return;
  warned = true;
  console.warn(
    "image inlining unavailable (emails will load images over the network):",
    reason instanceof Error ? reason.message : reason,
  );
}

/**
 * Normalize one raw markup reference to the absolute http(s) URL we would
 * fetch, or null when it is not a network image. Protocol-relative `//host/x`
 * resolves to https (the srcdoc origin would resolve it against http/https
 * anyway). `data:`, `cid:` and relative refs are not network fetches and are
 * left alone by every path in this module.
 */
export function normalizeImageUrl(raw: string): string | null {
  const s = raw.trim();
  if (!s) return null;
  if (s.startsWith("//")) return "https:" + s;
  return /^https?:\/\//i.test(s) ? s : null;
}

/** Every `url(...)` target inside a CSS string (inline style attribute). */
const CSS_URL_RE = /url\(\s*(['"]?)([^'")]+)\1\s*\)/gi;

function eachCssUrl(css: string, fn: (raw: string) => void): void {
  CSS_URL_RE.lastIndex = 0;
  let m: RegExpExecArray | null;
  while ((m = CSS_URL_RE.exec(css)) !== null) fn(m[2]);
}

/**
 * Every distinct network image URL a message would fetch, in document order:
 *
 *   * `<img src>`  — the obvious one.
 *   * `<img srcset>` / `<source srcset>` — stripped by ammonia today, covered
 *     defensively so a widened allow-list can't silently reopen the hole.
 *   * `background="…"` attributes — likewise stripped server-side today.
 *   * `url(…)` inside any inline `style` attribute — NOT stripped, extremely
 *     common in table-layout marketing mail, and the reference class the old
 *     warmer missed entirely.
 *
 * <style> blocks are deliberately not scanned: ammonia removes them at ingest,
 * so a stored message cannot contain one.
 *
 * DOMParser-based with the same graceful degradation as stripTrackers — if
 * parsing is unavailable or throws, this returns [] and callers simply fall
 * back to network loading.
 */
export function collectImageUrls(html: string): string[] {
  if (typeof DOMParser === "undefined") return [];
  const out: string[] = [];
  const seen = new Set<string>();
  const push = (raw: string) => {
    if (out.length >= MAX_PER_MESSAGE) return;
    const url = normalizeImageUrl(raw);
    if (!url || seen.has(url)) return;
    seen.add(url);
    out.push(url);
  };

  try {
    const doc = new DOMParser().parseFromString(html, "text/html");
    for (const el of Array.from(doc.querySelectorAll("[src],[srcset],[background],[style]"))) {
      const tag = el.tagName.toLowerCase();
      if (tag === "img") push(el.getAttribute("src") ?? "");
      // A srcset is a comma-separated list of "<url> <descriptor>" candidates.
      const srcset = el.getAttribute("srcset");
      if (srcset) {
        for (const part of srcset.split(",")) push(part.trim().split(/\s+/)[0] ?? "");
      }
      push(el.getAttribute("background") ?? "");
      const style = el.getAttribute("style");
      if (style) eachCssUrl(style, push);
    }
  } catch {
    return [];
  }
  return out;
}

/** Move an existing entry to the MRU end of the LRU order. */
function touch(url: string, value: string): void {
  cache.delete(url);
  cache.set(url, value);
}

function store(url: string, dataUrl: string): void {
  if (dataUrl.length > MAX_IMAGE_CHARS) return; // too big to be worth the budget
  const prev = cache.get(url);
  if (prev !== undefined) {
    cache.delete(url);
    cacheChars -= prev.length;
  }
  cache.set(url, dataUrl);
  cacheChars += dataUrl.length;
  while (cacheChars > MAX_CACHE_CHARS) {
    const oldest = cache.keys().next().value;
    if (oldest === undefined || oldest === url) break;
    const evicted = cache.get(oldest);
    cache.delete(oldest);
    cacheChars -= evicted?.length ?? 0;
  }
}

function pump(): void {
  while (active < MAX_CONCURRENT && queue.length > 0) {
    // Urgent first (an open in progress), otherwise FIFO.
    let i = queue.findIndex((j) => j.urgent);
    if (i < 0) i = 0;
    const [job] = queue.splice(i, 1);
    active++;
    job.start();
  }
}

/** True when a rejection means the command itself isn't there (stale shell). */
function looksMissing(e: unknown): boolean {
  const msg = String(e instanceof Error ? e.message : e).toLowerCase();
  return msg.includes("not found") || msg.includes("not allowed");
}

/**
 * The data URI for one image URL — from cache, or fetched through the shell.
 * Resolves null on any failure (never throws). Concurrent callers for the same
 * URL share one fetch; `urgent` promotes an already-queued background job.
 */
export function fetchImageDataUrl(
  url: string,
  urgent = false,
): Promise<string | null> {
  const hit = cache.get(url);
  if (hit !== undefined) {
    touch(url, hit);
    return Promise.resolve(hit);
  }
  if (unavailable) return Promise.resolve(null);
  const failedAt = failures.get(url);
  if (failedAt !== undefined && Date.now() - failedAt < FAILURE_TTL_MS) {
    return Promise.resolve(null);
  }
  const existing = inflight.get(url);
  if (existing) {
    if (urgent) {
      const queued = queue.find((j) => j.url === url);
      if (queued) queued.urgent = true;
    }
    return existing;
  }

  const p = new Promise<string | null>((resolve) => {
    queue.push({
      url,
      urgent,
      start: () => {
        invoke<string>("fetch_image_data_url", { url })
          .then((dataUrl) => {
            store(url, dataUrl);
            failures.delete(url);
            resolve(dataUrl);
          })
          .catch((e) => {
            if (looksMissing(e)) markUnavailable(e);
            failures.set(url, Date.now());
            resolve(null);
          })
          .finally(() => {
            active--;
            inflight.delete(url);
            pump();
          });
      },
    });
  });
  inflight.set(url, p);
  pump();
  return p;
}

/**
 * Fetch every image a message references into the cache. Resolves when they
 * have all settled (success OR failure) — never rejects. `urgent` is for the
 * message being opened right now; inbox warming leaves it false so an open
 * always cuts the line.
 */
export function warmImageCache(
  html: string,
  opts?: { urgent?: boolean },
): Promise<void> {
  if (unavailable) return Promise.resolve();
  const urls = collectImageUrls(html);
  if (urls.length === 0) return Promise.resolve();
  return Promise.all(
    urls.map((u) => fetchImageDataUrl(u, opts?.urgent ?? false)),
  ).then(() => undefined);
}

/**
 * True when nothing in this message is still worth waiting for: every
 * reference is either cached or known-failed. Used by the frame to decide
 * between rendering immediately and holding paint for the warm to land.
 */
export function imagesSettled(html: string): boolean {
  if (unavailable) return true;
  const now = Date.now();
  for (const url of collectImageUrls(html)) {
    if (cache.has(url)) continue;
    const failedAt = failures.get(url);
    if (failedAt !== undefined && now - failedAt < FAILURE_TTL_MS) continue;
    return false;
  }
  return true;
}

/** True when a message references at least one network image (any form). */
export function hasNetworkImages(html: string): boolean {
  return collectImageUrls(html).length > 0;
}

/**
 * Rewrite every cached image reference in `html` to its `data:` URI. Uncached
 * references are left EXACTLY as they were, so a partially-warmed message still
 * renders — those images just load over the network as before.
 *
 * Returns the rewritten html plus how many references were inlined vs. left
 * remote (`remote === 0` means the frame will make no network requests at all,
 * which is what the caller needs to know to tighten the frame CSP).
 */
export function inlineImages(html: string): {
  html: string;
  inlined: number;
  remote: number;
} {
  if (typeof DOMParser === "undefined") return { html, inlined: 0, remote: 0 };

  let inlined = 0;
  let remote = 0;
  /** Cached data URI for a raw markup ref, or null to leave it untouched. */
  const resolve = (raw: string): string | null => {
    const url = normalizeImageUrl(raw);
    if (!url) return null; // data:/cid:/relative — not a network fetch
    const hit = cache.get(url);
    if (hit === undefined) {
      remote++;
      return null;
    }
    touch(url, hit);
    inlined++;
    return hit;
  };

  try {
    const doc = new DOMParser().parseFromString(html, "text/html");
    for (const el of Array.from(doc.querySelectorAll("[src],[srcset],[background],[style]"))) {
      let srcInlined = false;
      if (el.tagName.toLowerCase() === "img") {
        const src = el.getAttribute("src");
        const dataUrl = src != null ? resolve(src) : null;
        if (dataUrl != null) {
          el.setAttribute("src", dataUrl);
          srcInlined = true;
          // A surviving srcset would OUTRANK the src we just inlined and pull
          // the image over the network anyway, defeating the whole point.
          el.removeAttribute("srcset");
        }
      }
      // A srcset is never REWRITTEN: its candidates are comma-separated, and a
      // data: URI is full of commas, so injecting one produces markup whose
      // parse is ambiguous at best. Any srcset we did not delete above is
      // therefore still a live network reference and must keep the frame's
      // remote CSP grant — silently tightening it would break the image.
      // (Ammonia strips srcset at ingest today; this is here so a widened
      // allow-list can't quietly turn into a broken frame.)
      if (!srcInlined && el.hasAttribute("srcset")) remote++;
      const background = el.getAttribute("background");
      if (background != null) {
        const dataUrl = resolve(background);
        if (dataUrl != null) el.setAttribute("background", dataUrl);
      }
      const style = el.getAttribute("style");
      if (style != null && style.includes("url(")) {
        // Base64 payloads contain no quote characters, so wrapping in double
        // quotes is always well-formed CSS here.
        const next = style.replace(CSS_URL_RE, (whole, _q: string, raw: string) => {
          const dataUrl = resolve(raw);
          return dataUrl != null ? `url("${dataUrl}")` : whole;
        });
        if (next !== style) el.setAttribute("style", next);
      }
    }
    if (inlined === 0) return { html, inlined: 0, remote };
    return { html: doc.body.innerHTML, inlined, remote };
  } catch {
    return { html, inlined: 0, remote: 0 };
  }
}

/** Test seam: drop all cached bytes and scheduler state. */
export function __resetImageCache(): void {
  cache.clear();
  failures.clear();
  inflight.clear();
  queue.length = 0;
  cacheChars = 0;
  active = 0;
  unavailable = !isTauriSafe();
  warned = false;
}
