// SITREP VIEW — the main chassis. OWNED BY: view-agent-1 (sitrep).
//
// Header (signal/noise + last checked) · STANDING / SINCE LAST CHECK / STILL
// OPEN bands · collapsed noise line · sealed lock-chip section. Keyboard-first:
// j/k traverse bands then sealed; Enter drills into a thread; r/e/d/t dispatch
// through lib/dispatch (the seam to ActionLayer); r on a sealed row reveals.
//
// Selection model: the store owns band selection (stable by message id, used by
// useSitrep + orderedIds). Sealed rows live outside orderedIds, so this view
// tracks a local "sealed focus" that j/k flows into past the last band row.

import { useEffect, useMemo, useState } from "react";
import { useStore } from "../state";
import { useKeys } from "../keys";
import type { AttentionUpdate, SealedMeta } from "../api";
import { SitrepHeader } from "../components/SitrepHeader";
import { BandSection } from "../components/BandSection";
import { NoiseLine } from "../components/NoiseLine";
import { SealedSection } from "../components/SealedSection";
import { RevealPanel } from "../components/RevealPanel";
import { flipTheme } from "../components/ThemeToggle";
import { ShortcutsOverlay } from "../components/ShortcutsOverlay";
import {
  dispatchArchive,
  dispatchDone,
  dispatchReply,
  dispatchTuneSender,
} from "../lib/dispatch";
import "../styles/sitrep.css";

export function SitrepView() {
  const sitrep = useStore((s) => s.sitrep);
  const refreshError = useStore((s) => s.refreshError);
  const selectedId = useStore((s) => s.selectedId);
  const select = useStore((s) => s.select);
  const move = useStore((s) => s.moveSelection);
  const orderedIds = useStore((s) => s.orderedIds);
  const openSide = useStore((s) => s.openSide);
  const fireUndo = useStore((s) => s.fireUndo);
  const selectedUpdate = useStore((s) => s.selectedUpdate);

  // Sealed focus lives here (sealed ids are disjoint from band message ids and
  // absent from orderedIds). null => focus is in the bands.
  const [sealedFocusId, setSealedFocusId] = useState<number | null>(null);
  // The one revealed sealed body currently on screen (held only while mounted).
  const [revealing, setRevealing] = useState<SealedMeta | null>(null);
  // Keyboard-shortcuts help overlay ('?').
  const [showShortcuts, setShowShortcuts] = useState(false);

  const sealed = sitrep.sealed;

  // Keep sealed focus valid across refreshes; drop it if that item vanished.
  useEffect(() => {
    if (sealedFocusId !== null && !sealed.some((m) => m.id === sealedFocusId)) {
      setSealedFocusId(null);
    }
  }, [sealed, sealedFocusId]);

  const openThread = (u: AttentionUpdate) =>
    openSide({ kind: "thread", threadId: u.thread_id });

  const revealSealed = (m: SealedMeta) => setRevealing(m);

  // --- j/k that spans bands then sealed --------------------------------------
  const moveDown = () => {
    if (sealedFocusId !== null) {
      const idx = sealed.findIndex((m) => m.id === sealedFocusId);
      if (idx < sealed.length - 1) setSealedFocusId(sealed[idx + 1].id);
      return;
    }
    const ids = orderedIds();
    const atLastBand =
      ids.length > 0 && selectedId === ids[ids.length - 1];
    if ((atLastBand || ids.length === 0) && sealed.length > 0) {
      // Cross the boundary into sealed.
      setSealedFocusId(sealed[0].id);
      return;
    }
    move(1);
  };

  const moveUp = () => {
    if (sealedFocusId !== null) {
      const idx = sealed.findIndex((m) => m.id === sealedFocusId);
      if (idx > 0) {
        setSealedFocusId(sealed[idx - 1].id);
      } else {
        // Back up into the last band row.
        setSealedFocusId(null);
        const ids = orderedIds();
        if (ids.length > 0) select(ids[ids.length - 1]);
      }
      return;
    }
    move(-1);
  };

  const bindings = useMemo(
    () => [
      { key: "j", description: "next", handler: () => moveDown() },
      { key: "k", description: "prev", handler: () => moveUp() },
      {
        key: "Enter",
        description: "drill in",
        handler: () => {
          const u = selectedUpdate();
          if (sealedFocusId === null && u) openThread(u);
        },
      },
      {
        key: "r",
        description: "reply / reveal",
        handler: () => {
          if (sealedFocusId !== null) {
            const m = sealed.find((s) => s.id === sealedFocusId);
            if (m) revealSealed(m);
            return;
          }
          const u = selectedUpdate();
          if (u) dispatchReply(u);
        },
      },
      {
        key: "e",
        description: "archive",
        handler: () => {
          if (sealedFocusId !== null) return;
          const u = selectedUpdate();
          if (u) void dispatchArchive(u);
        },
      },
      {
        key: "d",
        description: "done",
        handler: () => {
          if (sealedFocusId !== null) return;
          const u = selectedUpdate();
          if (u) void dispatchDone(u);
        },
      },
      {
        key: "t",
        description: "tune sender",
        handler: () => {
          if (sealedFocusId !== null) return;
          const u = selectedUpdate();
          if (u) dispatchTuneSender(u);
        },
      },
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
    // Recompute when selection zone / sealed set changes so closures stay fresh.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [sealedFocusId, sealed, selectedId],
  );
  useKeys("list", bindings, [bindings]);

  const onSelectBand = (id: number) => {
    setSealedFocusId(null);
    select(id);
  };

  const noiseCount =
    sitrep.stats?.tier_counts?.noise ?? 0;

  return (
    <div className="sitrep">
      <SitrepHeader
        stats={sitrep.stats}
        standingCount={sitrep.standing.length}
        newCount={sitrep.new.length}
        openCount={sitrep.open.length}
        refreshError={refreshError}
        onShowShortcuts={() => setShowShortcuts(true)}
      />

      <BandSection
        variant="standing"
        items={sitrep.standing}
        selectedId={sealedFocusId === null ? selectedId : null}
        onSelect={onSelectBand}
        onOpen={openThread}
      />
      <BandSection
        variant="new"
        items={sitrep.new}
        selectedId={sealedFocusId === null ? selectedId : null}
        onSelect={onSelectBand}
        onOpen={openThread}
      />
      <BandSection
        variant="open"
        items={sitrep.open}
        selectedId={sealedFocusId === null ? selectedId : null}
        onSelect={onSelectBand}
        onOpen={openThread}
      />

      <NoiseLine
        noiseCount={noiseCount}
        onBrowse={() => openSide({ kind: "browse" })}
        onRules={() => openSide({ kind: "rules" })}
      />

      <SealedSection
        items={sealed}
        selectedId={sealedFocusId}
        onSelect={setSealedFocusId}
        onReveal={revealSealed}
      />

      {revealing && (
        <RevealPanel meta={revealing} onClose={() => setRevealing(null)} />
      )}

      {showShortcuts && (
        <ShortcutsOverlay onClose={() => setShowShortcuts(false)} />
      )}
    </div>
  );
}
