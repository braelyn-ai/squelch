// Thread prefetch + image warmer — the "instant open" machinery.
//
// The thread viewer used to cold-start on every open: fetch the thread, build
// the srcdoc, then let images trickle in over the network. This module lets the
// inbox warm all of that BEFORE the click:
//   - prefetchThread(id): fetch + LRU-cache the ClientThreadView, then warm
//     every image it references into lib/imageCache (Rust-side fetch, held as
//     data: URIs) so the frame can be built with the bytes already in hand.
//   - getPrefetchedThread(id): a fresh cached view (or null) — the viewer
//     renders it synchronously on mount, no loading flash, no refetch.
//
// WARMING WAS REWRITTEN (2026-07). It used to create parent-document Image()
// objects and hope the shared WKWebView HTTP cache held them — capped at 12
// <img src> per message and blind to CSS url(). Both holes are why image-heavy
// mail still flickered on open; see the header of lib/imageCache for the full
// account. Warming now covers EVERY reference in EVERY message of the thread,
// and the bytes are pinned rather than hoped for. The per-message cap is gone:
// lib/imageCache's global byte budget is the real limiter, so coverage is
// bounded by memory rather than by an arbitrary count that silently truncated
// exactly the below-the-fold images that do the reflowing.
//
// PRIVACY: warming loads remote images for mail the user hasn't opened, so it
// (a) strips tracking pixels first (same stripTrackers pass the frame uses),
// (b) never runs when the Settings "load on demand" images pref is on, and
// (c) sends no referrer (the shell fetch carries no cookies or Referer).

import { api } from "../api";
import type { ClientThreadView } from "../api";
import { getPrefs } from "./prefs";
import { stripTrackers } from "./trackers";
import { warmImageCache } from "./imageCache";

/* Sized to hold the whole For-your-eyes list (sitrep preloads every standing
   item) plus inbox hover-warms without LRU churn. Views are small (text/html
   strings); 60 is a few MB worst-case. */
const CACHE_MAX = 60;
/** A cached view older than this refetches on real open (mail can change). */
const FRESH_MS = 60_000;
const cache = new Map<
  string,
  { view: ClientThreadView; ts: number; freshMs: number }
>();
const inflight = new Set<string>();
/** Promise-shaped in-flight dedupe for callers that need the VIEW back
 *  (newsletter hero thumbnails) rather than fire-and-forget warming. */
const inflightPromises = new Map<string, Promise<ClientThreadView>>();

/**
 * Pin every image in every message of a view (pref-gated). Fire-and-forget:
 * lib/imageCache dedupes by URL, negative-caches failures and bounds its own
 * concurrency, so re-warming an already-warm thread costs a map lookup per
 * reference. Warming is deliberately NOT urgent — an email actually being
 * opened promotes its own fetches ahead of this background sweep.
 */
function warmImages(view: ClientThreadView): void {
  if (!getPrefs().loadRemoteImages) return;
  for (const msg of view.messages) {
    if (!msg.html) continue;
    const { html } = stripTrackers(msg.html);
    void warmImageCache(html);
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

/**
 * Fetch a thread THROUGH the cache, returning the view: a fresh cache hit
 * resolves immediately; concurrent callers share one request. Used by the
 * newsletter hero thumbnails (they need the html, not just warming).
 */
export function fetchThreadCached(
  threadId: string,
  opts?: { freshMs?: number },
): Promise<ClientThreadView> {
  const freshMs = opts?.freshMs ?? FRESH_MS;
  const hit = cache.get(threadId);
  if (hit && Date.now() - hit.ts < Math.max(hit.freshMs, freshMs)) {
    if (freshMs > hit.freshMs) hit.freshMs = freshMs;
    return Promise.resolve(hit.view);
  }
  const pending = inflightPromises.get(threadId);
  if (pending) return pending;
  const p = api
    .getThread(threadId)
    .then((view) => {
      put(threadId, view, freshMs);
      warmImages(view);
      return view;
    })
    .finally(() => inflightPromises.delete(threadId));
  inflightPromises.set(threadId, p);
  return p;
}

/** Let the viewer's own (authoritative) fetch feed the cache + warm images —
 *  the next reopen of the same thread is then instant too. */
export function noteFetchedThread(threadId: string, view: ClientThreadView): void {
  put(threadId, view, FRESH_MS);
  warmImages(view);
}
