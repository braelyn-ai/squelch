// App shell. Boots settings from the keyring, shows Connect until connected,
// then mounts the sitrep chassis + the three view surfaces the next agents fill.

import { useEffect } from "react";
import { useStore, useSitrep } from "./state";
import { Connect } from "./views/Connect";
import { SitrepView } from "./views/SitrepView";
import { ActionLayer } from "./views/ActionLayer";
import { SideViews } from "./views/SideViews";

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

  return (
    <div className="app-shell">
      <SitrepView />
      {/* Side panel surfaces (thread / rules / browse / search). */}
      <SideViews />
      {/* Global overlay: undo toasts, compose ceremony, palette. */}
      <ActionLayer />
    </div>
  );
}
