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
import { SettingsView } from "./views/SettingsView";
import { UsageView } from "./views/UsageView";
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
  const goBack = useStore((s) => s.goBack);
  const goForward = useStore((s) => s.goForward);

  // 1..5 view nav + cmd+[ back / cmd+] forward. Registered in the "global"
  // context so they compose with the active context (list / sitrep / modal)
  // rather than being gated by it — nav must always work, even from a routed
  // panel. Digits are otherwise unbound; the ⌘ chords use the new `meta` flag so
  // a bare "[" / "]" never triggers them. allowInInput keeps history nav working
  // even with a search/compose field focused (it's a chord, not a typed char).
  const navBindings = useMemo(
    () => [
      ...MAIN_VIEWS.map((view, i) => ({
        key: String(i + 1),
        description: `go to ${view}`,
        handler: () => setView(view),
      })),
      {
        key: "[",
        meta: true,
        allowInInput: true,
        description: "back",
        handler: () => goBack(),
      },
      {
        key: "]",
        meta: true,
        allowInInput: true,
        description: "forward",
        handler: () => goForward(),
      },
    ],
    [setView, goBack, goForward],
  );
  useKeys("global", navBindings, [navBindings]);

  return (
    <div className="app-shell">
      {/* macOS overlay-titlebar drag region: a slim, non-interactive top strip.
          Only over empty chrome — the rail/header controls sit above it in the
          normal flow and are not covered by an interactive area, so their clicks
          are never swallowed. data-tauri-drag-region makes the OS treat it as
          the titlebar for window drag. */}
      <div className="drag-strip" data-tauri-drag-region aria-hidden="true" />
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
    case "usage":
      return <UsageView />;
    case "settings":
      return <SettingsView />;
    default:
      return <SitrepView />;
  }
}

