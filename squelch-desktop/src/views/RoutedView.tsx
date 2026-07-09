// ROUTED VIEW HOST — Auth / Rules / Audit promoted from side panels to full
// main views behind the sidebar rail.
//
// These three inner components (AuthView / RulesView / AuditView) were written
// for the SidePanel contract: they register their list-style keys into the
// "modal" KeyContext via useKeys("modal", …) and never push a context
// themselves. To reuse them UNCHANGED as routed main views, this host pushes
// the "modal" context while mounted — exactly like SidePanel did — so their
// j/k/n/e/x/r/Enter bindings light up. Only one routed view is mounted at a
// time (see App.RouteBody), so there's never a competing "list" set active.
//
// The global 1..5 view-nav keys (registered in App on the "global" context)
// keep firing here because "global" composes with "modal" in dispatchCore.

import { useKeyContext } from "../keys";
import { AuthView } from "../components/AuthView";
import { RulesView } from "../components/RulesView";
import { AuditView } from "../components/AuditView";
import "../styles/sitrep.css";

type RoutedKind = "auth" | "rules" | "audit";

const TITLE: Record<RoutedKind, string> = {
  auth: "Auth — login codes & alerts",
  rules: "Rules — sender rules",
  audit: "Audit — agent & app actions",
};

export function RoutedView({ view }: { view: RoutedKind }) {
  // Push the "modal" context the inner views register into (they call
  // useKeys("modal", …) and must NOT push a second context themselves).
  useKeyContext("modal");

  return (
    <div className="routed-view">
      <header className="routed-head">
        <h2>{TITLE[view]}</h2>
      </header>
      <div className="routed-body">
        {view === "auth" && <AuthView />}
        {view === "rules" && <RulesView />}
        {view === "audit" && <AuditView />}
      </div>
    </div>
  );
}
