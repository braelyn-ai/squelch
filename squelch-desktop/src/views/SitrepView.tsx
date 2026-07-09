// SITREP VIEW — the main chassis. OWNED BY: view-agent-1 (sitrep).
//
// Header (signal/noise + last checked) · STANDING / SINCE LAST CHECK / STILL
// OPEN bands · collapsed noise line · a compact "auth messages" pill. Auth mail
// (login codes / password resets / sign-in alerts) lives in its own side view
// (the Auth tab, `g`), not inline — the pill just notices it and opens the tab.
// Keyboard-first: j/k traverse bands; Enter drills into a thread; r/e/d/t
// dispatch through lib/dispatch (the seam to ActionLayer).

import { useMemo, useState } from "react";
import { useStore } from "../state";
import { useKeys } from "../keys";
import type { AttentionUpdate } from "../api";
import { SitrepHeader } from "../components/SitrepHeader";
import { BandSection } from "../components/BandSection";
import { NoiseLine } from "../components/NoiseLine";
import { flipTheme } from "../components/ThemeToggle";
import { ShortcutsOverlay } from "../components/ShortcutsOverlay";
import {
  dispatchArchive,
  dispatchDone,
  dispatchReply,
} from "../lib/dispatch";
import "../styles/sitrep.css";

export function SitrepView() {
  const sitrep = useStore((s) => s.sitrep);
  const refreshError = useStore((s) => s.refreshError);
  const selectedId = useStore((s) => s.selectedId);
  const select = useStore((s) => s.select);
  const move = useStore((s) => s.moveSelection);
  const openSide = useStore((s) => s.openSide);
  const fireUndo = useStore((s) => s.fireUndo);
  const selectedUpdate = useStore((s) => s.selectedUpdate);

  // Keyboard-shortcuts help overlay ('?').
  const [showShortcuts, setShowShortcuts] = useState(false);

  const authCount = sitrep.sealed.length;

  const openThread = (u: AttentionUpdate) =>
    openSide({ kind: "thread", threadId: u.thread_id });

  const openAuth = () => openSide({ kind: "auth" });

  const bindings = useMemo(
    () => [
      { key: "j", description: "next", handler: () => move(1) },
      { key: "k", description: "prev", handler: () => move(-1) },
      {
        key: "Enter",
        description: "drill in",
        handler: () => {
          const u = selectedUpdate();
          if (u) openThread(u);
        },
      },
      {
        key: "r",
        description: "reply",
        handler: () => {
          const u = selectedUpdate();
          if (u) dispatchReply(u);
        },
      },
      {
        key: "e",
        description: "archive",
        handler: () => {
          const u = selectedUpdate();
          if (u) void dispatchArchive(u);
        },
      },
      {
        key: "d",
        description: "done",
        handler: () => {
          const u = selectedUpdate();
          if (u) void dispatchDone(u);
        },
      },
      // NOTE: `t` (tune sender) is registered by ActionLayer, which owns the tune
      // overlay. It used to be double-registered here too; removed to avoid a
      // silent collision in the "list" context.
      {
        key: "a",
        description: "browse all",
        handler: () => openSide({ kind: "browse" }),
      },
      {
        key: "T",
        description: "rules",
        handler: () => openSide({ kind: "rules" }),
      },
      {
        key: "g",
        description: "auth messages",
        handler: () => openAuth(),
      },
      {
        key: "/",
        description: "search",
        handler: () => openSide({ kind: "search", query: "" }),
      },
      { key: "u", description: "undo", handler: () => void fireUndo() },
      {
        key: "\\",
        description: "toggle light/dark theme",
        handler: () => flipTheme(),
      },
      {
        key: "?",
        description: "keyboard shortcuts",
        handler: () => setShowShortcuts((v) => !v),
      },
    ],
    // Recompute when selection changes so closures stay fresh.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [selectedId],
  );
  useKeys("list", bindings, [bindings]);

  const onSelectBand = (id: number) => select(id);

  const noiseCount =
    sitrep.stats?.tier_counts?.noise ?? 0;

  return (
    <div className="sitrep">
      <SitrepHeader
        stats={sitrep.stats}
        standingCount={sitrep.standing.length}
        newCount={sitrep.new.length}
        openCount={sitrep.open.length}
        authCount={authCount}
        refreshError={refreshError}
        onShowShortcuts={() => setShowShortcuts(true)}
        onOpenAuth={openAuth}
      />

      <BandSection
        variant="standing"
        items={sitrep.standing}
        selectedId={selectedId}
        onSelect={onSelectBand}
        onOpen={openThread}
      />
      <BandSection
        variant="new"
        items={sitrep.new}
        selectedId={selectedId}
        onSelect={onSelectBand}
        onOpen={openThread}
      />
      <BandSection
        variant="open"
        items={sitrep.open}
        selectedId={selectedId}
        onSelect={onSelectBand}
        onOpen={openThread}
      />

      <NoiseLine
        noiseCount={noiseCount}
        authCount={authCount}
        onBrowse={() => openSide({ kind: "browse" })}
        onRules={() => openSide({ kind: "rules" })}
        onOpenAuth={openAuth}
      />

      {showShortcuts && (
        <ShortcutsOverlay onClose={() => setShowShortcuts(false)} />
      )}
    </div>
  );
}
