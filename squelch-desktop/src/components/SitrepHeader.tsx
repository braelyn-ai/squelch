// The sitrep header: brand line + signal/noise counts + "last checked: Xh ago".
// Signal = surfaced items (bands + tier signal), noise = squelched tier count.
// Pulls straight from store.sitrep.stats; degrades gracefully before first poll.

import type { StoreStats } from "../api";
import { lastChecked } from "../lib/format";

export interface SitrepHeaderProps {
  stats: StoreStats | null;
  standingCount: number;
  newCount: number;
  openCount: number;
  refreshError: string | null;
}

export function SitrepHeader({
  stats,
  standingCount,
  newCount,
  openCount,
  refreshError,
}: SitrepHeaderProps) {
  const signal = standingCount + newCount + openCount;
  const noise = stats?.tier_counts?.noise ?? 0;

  return (
    <header className="sitrep-header num">
      <span className="brand">
        squelch <span className="dim">· sitrep</span>
      </span>
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
    </header>
  );
}
