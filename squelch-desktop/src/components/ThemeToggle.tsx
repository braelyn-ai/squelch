// The header sun/moon theme toggle. Reflects and drives the document theme via
// the theme module. A small pub/sub keeps the button icon in sync when the
// theme is flipped from elsewhere (the '\' keybinding in SitrepView).

import { useEffect, useState } from "react";
import { Sun, Moon } from "lucide-react";
import { applyTheme, currentTheme, toggleTheme, type Theme } from "../state/theme";

// Module-level listeners so any toggle (button or keybind) notifies all mounts.
const listeners = new Set<(t: Theme) => void>();

/** Flip the theme and notify subscribers. Call this from the keymap too. */
export function flipTheme(): Theme {
  const next = toggleTheme();
  listeners.forEach((fn) => fn(next));
  return next;
}

/** Apply an explicit theme and notify subscribers (Settings' real control). */
export function setThemeTo(theme: Theme): Theme {
  applyTheme(theme);
  listeners.forEach((fn) => fn(theme));
  return theme;
}

/** Subscribe to theme changes; returns an unsubscribe. Keeps mounts in sync. */
export function subscribeTheme(fn: (t: Theme) => void): () => void {
  listeners.add(fn);
  return () => {
    listeners.delete(fn);
  };
}

export function ThemeToggle() {
  const [theme, setTheme] = useState<Theme>(() => currentTheme());

  useEffect(() => subscribeTheme(setTheme), []);

  const dark = theme === "dark";
  return (
    <button
      type="button"
      className="theme-toggle"
      onClick={() => flipTheme()}
      title={`${dark ? "light" : "dark"} mode (\\)`}
      aria-label={`switch to ${dark ? "light" : "dark"} mode`}
    >
      {dark ? <Sun size={16} /> : <Moon size={16} />}
    </button>
  );
}
