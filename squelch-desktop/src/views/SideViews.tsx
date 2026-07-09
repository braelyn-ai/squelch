// SIDE VIEWS — thread drill-in, rules audit, browse-all, search, audit log.
// OWNED BY: view-agent-1 (read side; shares the file-ownership map with agent-2's
// ActionLayer for overlays).
//
// Renders whichever side view store.sideView selects as a right-hand panel and
// owns the modal KeyContext + Esc-to-close for the whole panel. Each inner view
// (ThreadPane/SearchView/BrowseView/RulesView/AuditView) registers its own
// list-style keys into this same modal context via useKeys("modal", ...); they
// must NOT push a second context.

import { useMemo } from "react";
import { useStore } from "../state";
import { useKeys, useKeyContext } from "../keys";
import type { SideView } from "../state";
import { ThreadPane } from "../components/ThreadPane";
import { SearchView } from "../components/SearchView";
import { BrowseView } from "../components/BrowseView";
import { RulesView } from "../components/RulesView";
import { AuditView } from "../components/AuditView";
import "../styles/sitrep.css";

export function SideViews() {
  const sideView = useStore((s) => s.sideView);

  // CRITICAL: only mount the panel (and thus push the "modal" KeyContext) while a
  // side view is actually open. This component is always mounted by <Main>, so if
  // the modal context were pushed here unconditionally it would sit on top of the
  // context stack forever — permanently gating out the entire "list" keymap
  // (j/k/Enter/t/T/e/d/…) and leaving Escape as the only working key. Pushing the
  // context lives in <SidePanel>, which mounts only when kind !== "none".
  if (sideView.kind === "none") return null;

  return <SidePanel sideView={sideView} />;
}

function SidePanel({ sideView }: { sideView: SideView }) {
  const close = useStore((s) => s.closeSide);

  useKeyContext("modal");
  const bindings = useMemo(
    () => [{ key: "Escape", description: "back", handler: () => close() }],
    [close],
  );
  useKeys("modal", bindings, [bindings]);

  return (
    <aside className="side">
      <div className="side-head">
        <h2>{titleFor(sideView)}</h2>
        <span className="close">
          <kbd>Esc</kbd> close
        </span>
      </div>
      <div className="side-body">
        <SideBody view={sideView} />
      </div>
    </aside>
  );
}

function SideBody({ view }: { view: SideView }) {
  switch (view.kind) {
    case "thread":
      return <ThreadPane threadId={view.threadId} />;
    case "search":
      return <SearchView initialQuery={view.query} />;
    case "browse":
      return <BrowseView />;
    case "rules":
      return <RulesView />;
    case "audit":
      return <AuditView />;
    default:
      return null;
  }
}

function titleFor(v: SideView): string {
  switch (v.kind) {
    case "thread":
      return "thread";
    case "rules":
      return "rules audit";
    case "browse":
      return "browse — all mail";
    case "search":
      return "search";
    case "audit":
      return "audit log";
    default:
      return "";
  }
}
