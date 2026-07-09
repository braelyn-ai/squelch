// SIDEBAR — the slim icon rail (~52px) that routes the primary views.
//
// Sitrep (the abstracted dashboard) / Emails (the band list) / Auth / Rules /
// Audit. Each is a lucide icon with a hover tooltip and an active-state accent;
// the whole rail is theme-aware (inherits the app's CSS vars). The number keys
// 1..5 (registered globally in App) mirror this order — so the rail and the
// keyboard stay in lockstep via MAIN_VIEWS.

import {
  Gauge,
  Mail,
  KeyRound,
  SlidersHorizontal,
  ScrollText,
  type LucideIcon,
} from "lucide-react";
import { useStore, MAIN_VIEWS, type MainView } from "../state";
import { AuthRings } from "./AuthRing";

interface RailItem {
  view: MainView;
  label: string;
  Icon: LucideIcon;
}

// Order MUST match MAIN_VIEWS (the 1..5 key mapping); asserted below.
const ITEMS: RailItem[] = [
  { view: "sitrep", label: "Sitrep", Icon: Gauge },
  { view: "emails", label: "Emails", Icon: Mail },
  { view: "auth", label: "Auth", Icon: KeyRound },
  { view: "rules", label: "Rules", Icon: SlidersHorizontal },
  { view: "audit", label: "Audit", Icon: ScrollText },
];

export function Sidebar() {
  const activeView = useStore((s) => s.activeView);
  const setView = useStore((s) => s.setView);
  const authCount = useStore((s) => s.sitrep.sealed.length);

  return (
    <nav className="sidebar" aria-label="views">
      {ITEMS.map(({ view, label, Icon }, i) => {
        const active = activeView === view;
        const num = MAIN_VIEWS.indexOf(view) + 1;
        return (
          <button
            key={view}
            type="button"
            className={`rail-btn${active ? " active" : ""}`}
            onClick={() => setView(view)}
            aria-current={active ? "page" : undefined}
            aria-label={`${label} (${num || i + 1})`}
            title={`${label} · ${num || i + 1}`}
          >
            <Icon size={20} />
            {view === "auth" && <AuthRings />}
            {view === "auth" && authCount > 0 && (
              <span className="rail-badge" aria-hidden="true">
                {authCount}
              </span>
            )}
            <span className="rail-tip" role="tooltip">
              {label} <kbd>{num || i + 1}</kbd>
            </span>
          </button>
        );
      })}
    </nav>
  );
}
