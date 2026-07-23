// App shell. Boots settings from the keyring, shows Connect until connected,
// then mounts the sidebar rail + the routed main view + the side-panel/overlay
// surfaces.
//
// Layout: a slim icon rail (Sidebar) on the left routes between the primary
// views (Sitrep dashboard / Emails band list / Auth / Rules / Audit) via
// store.activeView. Number keys 1..5 (registered here in the GLOBAL key
// context, so they fire from every view including modal panels) mirror the
// rail order. The thread drill-in is the fullscreen ThreadViewer (layered above
// the side panels); reveal, rule editor, compose and process mode remain
// SidePanel/overlay surfaces, orthogonal to the routed view.

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
import { ThreadViewer } from "./components/ThreadViewer";
import {
  ConnectionBanner,
  DaemonDownPane,
} from "./components/ConnectionHealth";
import { Sidebar } from "./components/Sidebar";
import { openAskBar } from "./components/askBarBus";

/** The invisible titlebar. data-tauri-drag-region is what makes it draggable:
 *  Tauri's injected handler calls startDragging() on mousedown when the target
 *  element carries the attribute (CSS app-region does nothing in WKWebView).
 *  Rendered in every app state so a frameless window can always be moved. */
function DragStrip() {
  return (
    <div className="drag-strip" data-tauri-drag-region aria-hidden="true" />
  );
}

export function App() {
  const connStatus = useStore((s) => s.connStatus);
  const loadSettings = useStore((s) => s.loadSettings);

  // Boot: read keyring once.
  useEffect(() => {
    void loadSettings();
  }, [loadSettings]);

  // The drag strip must exist in EVERY state (loading / connect / main) — a
  // hidden-titlebar window with no strip mounted cannot be moved at all.
  if (connStatus === "loading") {
    return (
      <>
        <DragStrip />
        <div className="connect">
          <div style={{ color: "var(--fg-dim)" }}>loading…</div>
        </div>
      </>
    );
  }

  if (connStatus !== "connected") {
    return (
      <>
        <DragStrip />
        <Connect />
      </>
    );
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

  // Daemon health. "Down" = refreshes failing AND nothing has ever loaded this
  // session — there is no data worth rendering, so the routed view is replaced
  // by the down pane (Settings excepted: it must stay reachable to fix the
  // token/URL). Once a sync HAS landed, failures degrade to a stale-data
  // banner instead and the (stale) view stays up.
  const refreshError = useStore((s) => s.refreshError);
  const lastRefresh = useStore((s) => s.lastRefresh);
  const daemonDown = refreshError !== null && lastRefresh === null;

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
      {
        // ⌘K opens the "ask your inbox" assistant from anywhere, including with a
        // field focused (it's a chord, not a typed char) — hence allowInInput.
        key: "k",
        meta: true,
        allowInInput: true,
        description: "ask your inbox",
        handler: () => openAskBar(),
      },
    ],
    [setView, goBack, goForward],
  );
  useKeys("global", navBindings, [navBindings]);

  return (
    <div className="app-shell">
      {/* macOS overlay-titlebar drag region: a slim top strip that acts as the
          invisible titlebar. Fixed at a z-index above every fullscreen layer
          (thread viewer / panels / overlays) so the window stays draggable no
          matter what is open — the top chrome band is interaction-free on all
          surfaces by design (see .drag-strip in global.css). Dragging comes
          from the data-tauri-drag-region attribute, not CSS. */}
      <DragStrip />
      <Sidebar />
      <div className="app-main">
        <ConnectionBanner />
        {daemonDown && activeView !== "settings" ? (
          <DaemonDownPane />
        ) : (
          <RouteBody view={activeView} />
        )}
      </div>
      {/* Side panel surfaces (browse / search). */}
      <SideViews />
      {/* Fullscreen email viewer — layered above the side panels, z-below the
          ActionLayer overlays so compose/undo/palette stay on top. */}
      <ThreadViewer />
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

