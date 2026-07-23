// Measured email-frame heights, remembered per message for the session.
//
// EmailFrame sizes its iframe to the srcdoc's exact content height, but the
// first render can't know that height until the document loads — historically
// it painted a 120px placeholder and then jumped. Remembering the last
// measured height per message id lets a reopened frame render at its final
// size IMMEDIATELY (zero resize); only never-before-seen mail pays a single
// adjustment, and EmailFrame hides that behind a paint-hold.
//
// Heights are for the default (quoted-history-collapsed) state — EmailFrame
// only writes the cache while collapsed. In-memory only: a stale height after
// a window resize self-corrects on the load measure.

const MAX = 500;
const heights = new Map<string, number>();

export function getFrameHeight(key: string): number | null {
  return heights.get(key) ?? null;
}

export function setFrameHeight(key: string, h: number): void {
  heights.delete(key);
  heights.set(key, h);
  if (heights.size > MAX) {
    const oldest = heights.keys().next().value;
    if (oldest !== undefined) heights.delete(oldest);
  }
}

/** Drop a remembered height — called when a message is marked done (it's
 *  leaving the working set; no reason to hold its measurement). */
export function clearFrameHeight(key: string): void {
  heights.delete(key);
}
