// The sitrep header: brand line + signal/noise counts + "last checked: Xh ago".
// Signal = surfaced items (bands + tier signal), noise = the filtered-out tier
// count. When auth mail is waiting, a compact chip opens the Auth tab (a login
// code arriving is worth noticing near the top). Pulls straight from
// store.sitrep.stats; degrades gracefully before first poll.

import { KeyRound } from "lucide-react";
import type { StoreStats } from "../api";
import { lastChecked } from "../lib/format";
import { ThemeToggle } from "./ThemeToggle";

export interface SitrepHeaderProps {
  stats: StoreStats | null;
  standingCount: number;
  newCount: number;
  openCount: number;
  authCount: number;
  refreshError: string | null;
  onShowShortcuts: () => void;
  onOpenAuth: () => void;
}

export function SitrepHeader({
  stats,
  standingCount,
  newCount,
  openCount,
  authCount,
  refreshError,
  onShowShortcuts,
  onOpenAuth,
}: SitrepHeaderProps) {
  const signal = standingCount + newCount + openCount;
  const noise = stats?.tier_counts?.noise ?? 0;

  return (
    <header className="sitrep-header">
      <span className="brand">
        squelch <span className="dim">· sitrep</span>
      </span>
      <span className="head-right">
        <span className="stat-line">
          <span className="signal">
            <b>{signal}</b> signal
          </span>
          <span className="noise">
            <b>{noise}</b> noise
          </span>
          {refreshError ? (
            <span className="refresh-err" title={refreshError}>
              · offline
            </span>
          ) : (
            <span>· last checked: {lastChecked(stats?.last_surfaced_at)}</span>
          )}
        </span>
        {authCount > 0 && (
          <button
            type="button"
            className="auth-chip"
            onClick={onOpenAuth}
            title="login codes, password resets & sign-in alerts (g)"
          >
            <KeyRound size={14} /> {authCount}
          </button>
        )}
        <button
          type="button"
          className="theme-toggle help-hint"
          onClick={onShowShortcuts}
          title="keyboard shortcuts (?)"
        >
          ?
        </button>
        <ThemeToggle />
      </span>
    </header>
  );
}
