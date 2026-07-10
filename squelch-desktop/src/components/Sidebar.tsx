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
  Activity,
  Settings,
  type LucideIcon,
} from "lucide-react";
import { useStore, MAIN_VIEWS, type MainView } from "../state";
import { AuthRings } from "./AuthRing";

interface RailItem {
  view: MainView;
  label: string;
  Icon: LucideIcon;
}

// TOP group — order MUST match MAIN_VIEWS (the 1..5 key mapping).
const ITEMS: RailItem[] = [
  { view: "sitrep", label: "Sitrep", Icon: Gauge },
  { view: "emails", label: "Emails", Icon: Mail },
  { view: "auth", label: "Auth", Icon: KeyRound },
  { view: "rules", label: "Rules", Icon: SlidersHorizontal },
  { view: "audit", label: "Audit", Icon: ScrollText },
];

// BOTTOM group — pinned below a spacer + hairline divider. Deliberately NOT part
// of the 1..5 sequence (see MAIN_VIEWS / BOTTOM_VIEWS in the store): reached by
// click only, so the top group's digit nav stays stable.
const BOTTOM_ITEMS: RailItem[] = [
  { view: "usage", label: "Usage", Icon: Activity },
  { view: "settings", label: "Settings", Icon: Settings },
];

export function Sidebar() {
  const activeView = useStore((s) => s.activeView);
  const setView = useStore((s) => s.setView);
  const authCount = useStore((s) => s.sitrep.sealed.length);

  function railButton({ view, label, Icon }: RailItem, keyNum: number | null) {
    const active = activeView === view;
    return (
      <button
        key={view}
        type="button"
        className={`rail-btn${active ? " active" : ""}`}
        onClick={() => setView(view)}
        aria-current={active ? "page" : undefined}
        aria-label={keyNum ? `${label} (${keyNum})` : label}
        title={keyNum ? `${label} · ${keyNum}` : label}
      >
        <Icon size={20} />
        {view === "auth" && <AuthRings />}
        {view === "auth" && authCount > 0 && (
          <span className="rail-badge" aria-hidden="true">
            {authCount}
          </span>
        )}
        <span className="rail-tip" role="tooltip">
          {label} {keyNum && <kbd>{keyNum}</kbd>}
        </span>
      </button>
    );
  }

  return (
    <nav className="sidebar" aria-label="views">
      {ITEMS.map((item) => railButton(item, MAIN_VIEWS.indexOf(item.view) + 1))}
      {/* Spacer pushes the bottom group down; divider visually separates it. */}
      <div className="rail-spacer" aria-hidden="true" />
      <div className="rail-divider" aria-hidden="true" />
      {BOTTOM_ITEMS.map((item) => railButton(item, null))}
    </nav>
  );
}
