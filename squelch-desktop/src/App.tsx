// App shell. Boots settings from the keyring, shows Connect until connected,
// then mounts the sidebar rail + the routed main view + the side-panel/overlay
// surfaces.
//
// Layout: a slim icon rail (Sidebar) on the left routes between the primary
// views (Sitrep dashboard / Emails band list / Auth / Rules / Audit) via
// store.activeView. Number keys 1..5 (registered here in the GLOBAL key
// context, so they fire from every view including modal panels) mirror the
// rail order. Thread drill-in, reveal, rule editor, compose and process mode
// remain SidePanel/overlay surfaces, orthogonal to the routed view.

import { useEffect, useMemo } from "react";
import { useStore, useSitrep, useAuthArrival } from "./state";
import { MAIN_VIEWS } from "./state";
import { useKeys } from "./keys";
import { Connect } from "./views/Connect";
import { SitrepView } from "./views/SitrepView";
import { EmailsView } from "./views/EmailsView";
import { RoutedView } from "./views/RoutedView";
import { ActionLayer } from "./views/ActionLayer";
import { SideViews } from "./views/SideViews";
import { Sidebar } from "./components/Sidebar";

export function App() {
  const connStatus = useStore((s) => s.connStatus);
  const loadSettings = useStore((s) => s.loadSettings);

  // Boot: read keyring once.
  useEffect(() => {
    void loadSettings();
  }, [loadSettings]);

  if (connStatus === "loading") {
    return (
      <div className="connect">
        <div style={{ color: "var(--fg-dim)" }}>loading…</div>
      </div>
    );
  }

  if (connStatus !== "connected") {
    return <Connect />;
  }

  return <Main />;
}

function Main() {
  // Polling hook drives the sitrep read model (10s + on focus).
  useSitrep();
  // Watches sealed metadata for fresh auth arrivals → ring + code modal.
  useAuthArrival();

  const activeView = useStore((s) => s.activeView);
  const setView = useStore((s) => s.setView);

  // 1..5 view nav. Registered in the "global" context so it composes with the
  // active context (list / sitrep / modal) rather than being gated by it —
  // switching views must always work, even from a routed panel. Digits are
  // otherwise unbound across the app.
  const navBindings = useMemo(
    () =>
      MAIN_VIEWS.map((view, i) => ({
        key: String(i + 1),
        description: `go to ${view}`,
        handler: () => setView(view),
      })),
    [setView],
  );
  useKeys("global", navBindings, [navBindings]);

  return (
    <div className="app-shell">
      <Sidebar />
      <div className="app-main">
        <RouteBody view={activeView} />
      </div>
      {/* Side panel surfaces (thread / browse / search). */}
      <SideViews />
      {/* Global overlay: undo toasts, compose ceremony, palette. */}
      <ActionLayer />
    </div>
  );
}

function RouteBody({ view }: { view: ReturnType<typeof useStore.getState>["activeView"] }) {
  switch (view) {
    case "sitrep":
      return <SitrepView />;
    case "emails":
      return <EmailsView />;
    case "auth":
      return <RoutedView view="auth" />;
    case "rules":
      return <RoutedView view="rules" />;
    case "audit":
      return <RoutedView view="audit" />;
    default:
      return <SitrepView />;
  }
}

