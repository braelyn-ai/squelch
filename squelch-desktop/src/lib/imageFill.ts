// Newsletter-thumb fill color: fetch the hero's bytes through the Tauri shell
// (Rust reqwest — no webview, no CORS taint), then read pixels from a
// same-origin blob and average them to one hex. Every failure path returns
// null (the card keeps its neutral fill); nothing ever throws to the caller.

import { invoke } from "@tauri-apps/api/core";

/** Decode the command's base64 payload to bytes. */
function b64ToBytes(b64: string): Uint8Array {
  const bin = atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

/**
 * DOMINANT color of an image, not the average: newsletter heroes are mostly
 * white margin, so a plain average washes out to near-white. 16x16 downsample,
 * skip transparent + near-white + near-black pixels (margins/backgrounds/
 * text), coarse-quantize the rest (3 bits/channel), take the biggest bucket's
 * true average. Falls back to the overall average when everything was
 * neutral (a genuinely white/black hero), then null on decode failure.
 */
async function dominantHex(blob: Blob): Promise<string | null> {
  const url = URL.createObjectURL(blob);
  try {
    const img = new Image();
    await new Promise<void>((resolve, reject) => {
      img.onload = () => resolve();
      img.onerror = () => reject(new Error("decode failed"));
      img.src = url;
    });
    const c = document.createElement("canvas");
    c.width = 16;
    c.height = 16;
    const ctx = c.getContext("2d");
    if (!ctx) return null;
    ctx.drawImage(img, 0, 0, 16, 16);
    const d = ctx.getImageData(0, 0, 16, 16).data;

    // Buckets keyed by 3-bit RGB; each accumulates true sums for a clean mean.
    const buckets = new Map<
      number,
      { r: number; g: number; b: number; n: number }
    >();
    let ar = 0,
      ag = 0,
      ab = 0,
      an = 0;
    for (let i = 0; i < d.length; i += 4) {
      const r = d[i],
        g = d[i + 1],
        b = d[i + 2],
        a = d[i + 3];
      if (a < 128) continue;
      ar += r;
      ag += g;
      ab += b;
      an += 1;
      const lum = 0.299 * r + 0.587 * g + 0.114 * b;
      if (lum > 235 || lum < 20) continue; // margins / text ink
      const key = ((r >> 5) << 6) | ((g >> 5) << 3) | (b >> 5);
      const bk = buckets.get(key) ?? { r: 0, g: 0, b: 0, n: 0 };
      bk.r += r;
      bk.g += g;
      bk.b += b;
      bk.n += 1;
      buckets.set(key, bk);
    }

    const hex = (v: number, n: number) =>
      Math.round(v / n)
        .toString(16)
        .padStart(2, "0");
    let best: { r: number; g: number; b: number; n: number } | null = null;
    for (const bk of buckets.values()) {
      if (!best || bk.n > best.n) best = bk;
    }
    if (best && best.n >= 8) {
      // A real dominant region (>= ~3% of samples).
      return `#${hex(best.r, best.n)}${hex(best.g, best.n)}${hex(best.b, best.n)}`;
    }
    if (an > 0) {
      return `#${hex(ar, an)}${hex(ag, an)}${hex(ab, an)}`;
    }
    return null;
  } catch {
    return null;
  } finally {
    URL.revokeObjectURL(url);
  }
}

/** The hero's average color as a hex string, or null (best-effort). */
export async function heroFillHex(src: string): Promise<string | null> {
  try {
    const b64 = await invoke<string>("fetch_image", { url: src });
    const bytes = b64ToBytes(b64);
    // Copy into a plain ArrayBuffer — TS's BlobPart won't take ArrayBufferLike.
    const buf = new ArrayBuffer(bytes.length);
    new Uint8Array(buf).set(bytes);
    return await dominantHex(new Blob([buf]));
  } catch (e) {
    // Non-tauri env (browser dev), a STALE SHELL BINARY (fetch_image ships
    // with the Rust side — full app restart required), or fetch failure.
    if (!warnedOnce) {
      warnedOnce = true;
      console.warn("hero fill unavailable:", e instanceof Error ? e.message : e);
    }
    return null;
  }
}

let warnedOnce = false;
