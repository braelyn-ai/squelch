// Local UI preferences — localStorage-backed, app-wide, reactive.
//
// Same persistence model as the theme (state/theme.ts): a tiny synchronous
// module over localStorage, no server round-trip — these are per-device view
// preferences, not account state. Components subscribe via usePref() (a
// useSyncExternalStore hook) so a change in Settings re-renders every open
// consumer (e.g. the email frames) immediately.

import { useSyncExternalStore } from "react";

const PREFS_KEY = "squelch-prefs";

/** The Settings sub-nav sections; the last-active one is restored on reopen. */
export type SettingsSection =
  | "general"
  | "mail"
  | "triage"
  | "assistant"
  | "account";

export interface Prefs {
  /** Load remote (network) images in email HTML automatically. When false,
   *  each message shows a per-email "load images" opt-in instead. */
  loadRemoteImages: boolean;
  /** Which Settings section was last open, so reopening restores it. */
  settingsSection: SettingsSection;
  /** Blend weight (0..1) for the Sitrep "For your eyes" ranking: the urgency
   *  (time) share of the score; the remainder is severity. 0 = rank purely by
   *  severity, 1 = purely by time. See lib/ranking.ts. */
  rankWeight: number;
}

const DEFAULTS: Prefs = {
  loadRemoteImages: true,
  settingsSection: "general",
  rankWeight: 0.6,
};

function read(): Prefs {
  try {
    const raw = localStorage.getItem(PREFS_KEY);
    if (!raw) return { ...DEFAULTS };
    const parsed = JSON.parse(raw) as Partial<Prefs>;
    return { ...DEFAULTS, ...parsed };
  } catch {
    return { ...DEFAULTS };
  }
}

let cache: Prefs = read();
const listeners = new Set<() => void>();

export function getPrefs(): Prefs {
  return cache;
}

export function setPref<K extends keyof Prefs>(key: K, value: Prefs[K]): void {
  cache = { ...cache, [key]: value };
  try {
    localStorage.setItem(PREFS_KEY, JSON.stringify(cache));
  } catch {
    // storage unavailable — the in-memory value still holds for this session.
  }
  for (const l of listeners) l();
}

function subscribe(cb: () => void): () => void {
  listeners.add(cb);
  return () => listeners.delete(cb);
}

/** Reactive read of one preference. */
export function usePref<K extends keyof Prefs>(key: K): Prefs[K] {
  return useSyncExternalStore(subscribe, () => cache[key]);
}
