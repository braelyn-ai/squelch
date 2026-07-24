// Save attachment bytes to a user-chosen path.
//
// TWO RUNTIMES (mirrors lib/opener.ts):
//   - Inside Tauri (the shipped app): the @tauri-apps/plugin-dialog save dialog
//     picks a path, then @tauri-apps/plugin-fs writes the bytes there. The fs
//     capability is scoped to the dialog-chosen path mechanism ONLY (no broad
//     read/write grant) — see src-tauri/capabilities/default.json.
//   - In browser-dev (vite, no Tauri): fall back to an <a download> + object URL
//     so the whole flow is exercisable in Chrome while iterating.
//
// The caller has ALREADY fetched the bytes over the authenticated door (bearer
// auth can't ride on an <img>/<a href>), so this only handles the save leg.

/** Detect the Tauri runtime without importing the API (safe in plain browser). */
function inTauri(): boolean {
  return (
    typeof window !== "undefined" &&
    "__TAURI_INTERNALS__" in (window as unknown as Record<string, unknown>)
  );
}

/**
 * Keep only the base name and drop control chars / quotes so the default save
 * name can't redirect out of the chosen directory or carry a control byte.
 * Empty result falls back to a generic name.
 */
export function sanitizeFilename(name: string): string {
  const base = name.split(/[\\/]/).pop() ?? name;
  // Drop control chars (anything below the space codepoint), quotes, and
  // Unicode bidi controls (U+202A-202E, U+2066-2069) — an RLO makes
  // "report\u202Efdp.exe" display as "report exe.pdf": extension spoofing.
  const cleaned = Array.from(base)
    .filter(
      (ch) =>
        ch >= " " &&
        ch !== '"' &&
        ch !== "'" &&
        !(ch >= "\u202A" && ch <= "\u202E") &&
        !(ch >= "\u2066" && ch <= "\u2069"),
    )
    .join("")
    .trim();
  return cleaned || "attachment";
}

export type SaveResult = "saved" | "cancelled";

/**
 * Prompt for a save location (default = the sanitized attachment name) and write
 * `bytes` there. Returns "cancelled" when the user dismisses the dialog, "saved"
 * on success. Throws on a genuine write failure so the caller can toast it.
 */
export async function saveBytes(
  bytes: ArrayBuffer,
  filename: string,
): Promise<SaveResult> {
  const name = sanitizeFilename(filename);
  const data = new Uint8Array(bytes);

  if (inTauri()) {
    const { save } = await import("@tauri-apps/plugin-dialog");
    const path = await save({ defaultPath: name });
    if (!path) return "cancelled";
    const { writeFile } = await import("@tauri-apps/plugin-fs");
    await writeFile(path, data);
    return "saved";
  }

  // Browser dev fallback: synthesize a download link.
  const blob = new Blob([data]);
  const url = URL.createObjectURL(blob);
  try {
    const a = document.createElement("a");
    a.href = url;
    a.download = name;
    document.body.appendChild(a);
    a.click();
    a.remove();
  } finally {
    // Give the browser a tick to start the download before revoking.
    setTimeout(() => URL.revokeObjectURL(url), 10_000);
  }
  return "saved";
}
