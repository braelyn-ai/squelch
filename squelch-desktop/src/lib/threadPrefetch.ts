// Thread prefetch + image warmer — the "instant open" machinery.
//
// The thread viewer used to cold-start on every open: fetch the thread, build
// the srcdoc, then let images trickle in over the network. This module lets the
// inbox warm all of that BEFORE the click:
//   - prefetchThread(id): fetch + LRU-cache the ClientThreadView, then warm its
//     remote images by creating parent-document Image() objects — the fetches
//     land in the shared WKWebView HTTP cache, so when the frame's <img> tags
//     ask for the same URLs they paint from cache.
//   - getPrefetchedThread(id): a fresh cached view (or null) — the viewer
//     renders it synchronously on mount, no loading flash, no refetch.
//
// PRIVACY: warming loads remote images for mail the user hasn't opened, so it
// (a) strips tracking pixels first (same stripTrackers pass the frame uses),
// (b) never runs when the Settings "load on demand" images pref is on, and
// (c) sends no referrer. The parent CSP already permits http(s) images.

import { api } from "../api";
import type { ClientThreadView } from "../api";
import { getPrefs } from "./prefs";
import { stripTrackers } from "./trackers";

/* Sized to hold the whole For-your-eyes list (sitrep preloads every standing
   item) plus inbox hover-warms without LRU churn. Views are small (text/html
   strings); 60 is a few MB worst-case. */
const CACHE_MAX = 60;
/** A cached view older than this refetches on real open (mail can change). */
const FRESH_MS = 60_000;
/** Max images warmed per message — a 60-image megamail shouldn't stampede. */
const WARM_PER_MESSAGE = 12;
/** Bound the warmed-URL memo so it can't grow without limit. */
const WARMED_MAX = 600;

const cache = new Map<
  string,
  { view: ClientThreadView; ts: number; freshMs: number }
>();
const inflight = new Set<string>();
const warmedUrls = new Set<string>();

/** Extract http(s) img srcs (protocol-relative resolved to https). */
function imageSrcs(html: string): string[] {
  const out: string[] = [];
  const re = /<img\b[^>]*\bsrc\s*=\s*["']([^"']+)["']/gi;
  let m: RegExpExecArray | null;
  while ((m = re.exec(html)) !== null && out.length < WARM_PER_MESSAGE) {
    let src = m[1].trim();
    if (src.startsWith("//")) src = "https:" + src;
    if (/^https?:\/\//i.test(src)) out.push(src);
  }
  return out;
}

/** Warm a view's remote images into the shared HTTP cache (pref-gated). */
function warmImages(view: ClientThreadView): void {
  if (!getPrefs().loadRemoteImages) return;
  for (const msg of view.messages) {
    if (!msg.html) continue;
    const { html } = stripTrackers(msg.html);
    for (const src of imageSrcs(html)) {
      if (warmedUrls.has(src)) continue;
      if (warmedUrls.size >= WARMED_MAX) warmedUrls.clear();
      warmedUrls.add(src);
      const img = new Image();
      img.referrerPolicy = "no-referrer";
      img.decoding = "async";
      img.src = src;
    }
  }
}

function put(threadId: string, view: ClientThreadView, freshMs: number): void {
  cache.delete(threadId); // re-insert => most-recent position
  cache.set(threadId, { view, ts: Date.now(), freshMs });
  if (cache.size > CACHE_MAX) {
    const oldest = cache.keys().next().value;
    if (oldest !== undefined) cache.delete(oldest);
  }
}

/** Fire-and-forget: fetch + cache + warm. Deduped while in flight; a fresh
 *  cache hit just re-warms (Image() from HTTP cache is ~free). */
export function prefetchThread(
  threadId: string,
  opts?: { freshMs?: number },
): void {
  // Per-entry TTL: right-rail records (banking/receipts) stay valid as long as
  // their column shows them, so their cached threads outlive the 60s default.
  // A repeat prefetch may EXTEND an entry's ttl (never shorten it).
  const freshMs = opts?.freshMs ?? FRESH_MS;
  const hit = cache.get(threadId);
  if (hit && Date.now() - hit.ts < Math.max(hit.freshMs, freshMs)) {
    if (freshMs > hit.freshMs) hit.freshMs = freshMs;
    warmImages(hit.view);
    return;
  }
  if (inflight.has(threadId)) return;
  inflight.add(threadId);
  api
    .getThread(threadId)
    .then((view) => {
      put(threadId, view, freshMs);
      warmImages(view);
    })
    .catch(() => {
      // Prefetch is best-effort; the real open will surface any error.
    })
    .finally(() => inflight.delete(threadId));
}

/** A fresh cached view for instant render, or null. */
export function getPrefetchedThread(threadId: string): ClientThreadView | null {
  const hit = cache.get(threadId);
  if (!hit || Date.now() - hit.ts >= hit.freshMs) return null;
  return hit.view;
}

/** Let the viewer's own (authoritative) fetch feed the cache + warm images —
 *  the next reopen of the same thread is then instant too. */
export function noteFetchedThread(threadId: string, view: ClientThreadView): void {
  put(threadId, view, FRESH_MS);
  warmImages(view);
}
