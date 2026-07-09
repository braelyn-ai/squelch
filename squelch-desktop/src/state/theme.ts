// Theme system. Two palettes (light default, dark) selected by a
// data-theme="light"|"dark" attribute on <html>. The choice persists in
// localStorage under THEME_KEY; index.html applies it before first paint to
// avoid a flash, so this module only has to keep the DOM + storage in sync at
// runtime (toggle button, keybinding).

export type Theme = "light" | "dark";

export const THEME_KEY = "squelch-theme";

/** Read the persisted theme; default light. Tolerates SSR-less/no-storage. */
export function getStoredTheme(): Theme {
  try {
    const v = localStorage.getItem(THEME_KEY);
    return v === "dark" ? "dark" : "light";
  } catch {
    return "light";
  }
}

/** The theme currently applied to the document (falls back to stored). */
export function currentTheme(): Theme {
  const attr = document.documentElement.getAttribute("data-theme");
  return attr === "dark" ? "dark" : attr === "light" ? "light" : getStoredTheme();
}

/** Apply a theme to <html> and persist it. */
export function applyTheme(theme: Theme): void {
  document.documentElement.setAttribute("data-theme", theme);
  try {
    localStorage.setItem(THEME_KEY, theme);
  } catch {
    // storage unavailable — DOM attribute still holds for this session.
  }
}

/** Flip light<->dark, persist, and return the new theme. */
export function toggleTheme(): Theme {
  const next: Theme = currentTheme() === "dark" ? "light" : "dark";
  applyTheme(next);
  return next;
}
