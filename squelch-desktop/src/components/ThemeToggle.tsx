// The header sun/moon theme toggle. Reflects and drives the document theme via
// the theme module. A small pub/sub keeps the button icon in sync when the
// theme is flipped from elsewhere (the '\' keybinding in SitrepView).

import { useEffect, useState } from "react";
import { currentTheme, toggleTheme, type Theme } from "../state/theme";

// Module-level listeners so any toggle (button or keybind) notifies all mounts.
const listeners = new Set<(t: Theme) => void>();

/** Flip the theme and notify subscribers. Call this from the keymap too. */
export function flipTheme(): Theme {
  const next = toggleTheme();
  listeners.forEach((fn) => fn(next));
  return next;
}

export function ThemeToggle() {
  const [theme, setTheme] = useState<Theme>(() => currentTheme());

  useEffect(() => {
    listeners.add(setTheme);
    return () => {
      listeners.delete(setTheme);
    };
  }, []);

  const dark = theme === "dark";
  return (
    <button
      type="button"
      className="theme-toggle"
      onClick={() => flipTheme()}
      title={`${dark ? "light" : "dark"} mode (\\)`}
      aria-label={`switch to ${dark ? "light" : "dark"} mode`}
    >
      {dark ? "☀" : "☾"}
    </button>
  );
}
