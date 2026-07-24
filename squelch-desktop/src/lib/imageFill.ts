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

/** Average the opaque pixels of an image (8x8 downsample) to a #rrggbb hex. */
async function averageHex(blob: Blob): Promise<string | null> {
  const url = URL.createObjectURL(blob);
  try {
    const img = new Image();
    await new Promise<void>((resolve, reject) => {
      img.onload = () => resolve();
      img.onerror = () => reject(new Error("decode failed"));
      img.src = url;
    });
    const c = document.createElement("canvas");
    c.width = 8;
    c.height = 8;
    const ctx = c.getContext("2d");
    if (!ctx) return null;
    ctx.drawImage(img, 0, 0, 8, 8);
    const d = ctx.getImageData(0, 0, 8, 8).data;
    let r = 0,
      g = 0,
      b = 0,
      n = 0;
    for (let i = 0; i < d.length; i += 4) {
      if (d[i + 3] < 128) continue;
      r += d[i];
      g += d[i + 1];
      b += d[i + 2];
      n += 1;
    }
    if (n === 0) return null;
    const hex = (v: number) =>
      Math.round(v / n)
        .toString(16)
        .padStart(2, "0");
    return `#${hex(r)}${hex(g)}${hex(b)}`;
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
    return await averageHex(new Blob([buf]));
  } catch {
    // Non-tauri env (browser dev) or fetch/decode failure — neutral fill.
    return null;
  }
}
