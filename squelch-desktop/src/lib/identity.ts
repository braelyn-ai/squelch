// The human's display name — used only for the Sitrep greeting ("Good
// afternoon, Braelyn"). Stored client-side in localStorage (set in Settings);
// there's no such field on the human door, and it's cosmetic. Empty => the
// greeting drops the name.

const KEY = "squelch.name";

export function getUserName(): string {
  try {
    return (localStorage.getItem(KEY) ?? "").trim();
  } catch {
    return "";
  }
}

export function setUserName(name: string): void {
  try {
    localStorage.setItem(KEY, name.trim());
  } catch {
    // cosmetic — best effort
  }
}
