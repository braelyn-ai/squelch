// ACTION DISPATCH — the canonical verb API the read views (SitrepView, process
// mode, side views) call. OWNED BY: view-agent-2 (actions).
//
// Design laws (UX-DIRECTIONS): undo-first. archive/done/label fire instantly
// (the api client already sends confirm:true). The row leaves its band the
// instant the key is pressed (optimistic), a 5s undo toast is queued, and the
// inverse call is the toast's `revert`. `send` is the only ceremony path and
// lives in the compose overlay, reached here via reply(). Nothing here logs the
// token or any sealed body.
//
// Usage:
//   const act = useActions();
//   act.archive(update);  act.done(update);  act.reopen(update);
//   act.reply(update);    act.tune(sender);  act.label(update, add, remove);
//
// Non-hook callers (e.g. process-mode reducers) can use the raw dispatch fns
// exported at the bottom, which read the store via useStore.getState().

import { useMemo } from "react";
import { api, ApiError } from "../api";
import type { AttentionUpdate } from "../api";
import { useStore } from "../state";
import { openRuleEditor } from "../components/ruleEditorBus";

const INBOX = "INBOX";

function errText(e: unknown, fallback: string): string {
  return e instanceof ApiError ? e.message : fallback;
}

/**
 * Optimistically pull a message id out of whatever band holds it and keep the
 * selection valid (advance to the next row, else the previous). Returns a
 * `restore` thunk that re-inserts the removed rows if the server call fails.
 */
function removeFromBands(messageId: number): () => void {
  const store = useStore.getState();
  const prev = store.sitrep;

  // Compute the next selection BEFORE mutating, using the flat order.
  const orderBefore = store.orderedIds();
  const posBefore = orderBefore.indexOf(messageId);

  const next = {
    standing: prev.standing.filter((u) => u.id !== messageId),
    new: prev.new.filter((u) => u.id !== messageId),
    open: prev.open.filter((u) => u.id !== messageId),
  };
  store.setSitrep(next);

  // Reselect: next id in the pruned flat order at the same slot, clamped.
  if (store.selectedId === messageId) {
    const orderAfter = [
      ...next.standing,
      ...next.new,
      ...next.open,
    ].map((u) => u.id);
    if (orderAfter.length === 0) {
      store.select(null);
    } else {
      const idx = Math.min(Math.max(posBefore, 0), orderAfter.length - 1);
      store.select(orderAfter[idx]);
    }
  }

  // The (rare) failure path: put the exact prior bands back.
  return () => {
    const cur = useStore.getState();
    cur.setSitrep({
      standing: prev.standing,
      new: prev.new,
      open: prev.open,
    });
    cur.select(messageId);
  };
}

/** Archive (undo-first): row leaves instantly, INBOX-relabel is the revert. */
async function dispatchArchive(u: AttentionUpdate): Promise<void> {
  const store = useStore.getState();
  const restore = removeFromBands(u.id);
  try {
    await api.actionArchive(u.id);
    store.pushUndo({
      kind: "archive",
      messageId: u.id,
      label: `archived ${u.sender}`,
      revert: async () => {
        // archive undo = re-add the INBOX label; then refresh happens on poll.
        await api.actionLabel(u.id, [INBOX], []);
      },
    });
  } catch (e) {
    restore();
    if (e instanceof ApiError && e.kind === "forbidden") {
      store.pushToast("no write credential · run squelchd auth --write", "error");
    } else {
      store.pushToast(errText(e, "archive failed"), "error");
    }
  }
}

/** Done (undo-first): status->done, revert resets status to open. */
async function dispatchDone(u: AttentionUpdate): Promise<void> {
  const store = useStore.getState();
  const restore = removeFromBands(u.id);
  try {
    await api.setStatus(u.id, "done");
    store.pushUndo({
      kind: "done",
      messageId: u.id,
      label: `done ${u.sender}`,
      revert: async () => {
        await api.setStatus(u.id, "open");
      },
    });
  } catch (e) {
    restore();
    store.pushToast(errText(e, "done failed"), "error");
  }
}

/** Reopen a done item (status->open). No undo toast; it's already a recovery. */
async function dispatchReopen(u: AttentionUpdate): Promise<void> {
  const store = useStore.getState();
  try {
    await api.setStatus(u.id, "open");
    store.pushToast(`reopened ${u.sender}`, "info");
  } catch (e) {
    store.pushToast(errText(e, "reopen failed"), "error");
  }
}

/**
 * Add/remove labels (undo-first for the common archive-restore inverse). The
 * row is NOT pulled from its band for a plain label edit (it stays visible);
 * only the revert is queued so `u` can take it back.
 */
async function dispatchLabel(
  u: AttentionUpdate,
  add: string[] = [],
  remove: string[] = [],
): Promise<void> {
  const store = useStore.getState();
  try {
    await api.actionLabel(u.id, add, remove);
    store.pushUndo({
      kind: "label",
      messageId: u.id,
      label: `labeled ${u.sender}`,
      // Inverse: swap add<->remove.
      revert: async () => {
        await api.actionLabel(u.id, remove, add);
      },
    });
  } catch (e) {
    if (e instanceof ApiError && e.kind === "forbidden") {
      store.pushToast("no write credential · run squelchd auth --write", "error");
    } else {
      store.pushToast(errText(e, "label failed"), "error");
    }
  }
}

/** Reply: open the compose/review ceremony prefilled from the update. */
function dispatchReply(u: AttentionUpdate): void {
  const store = useStore.getState();
  store.openCompose({
    replyToMessageId: u.id,
    to: u.sender,
    subject: u.one_line.toLowerCase().startsWith("re:")
      ? u.one_line
      : `Re: ${u.one_line}`,
    body: "",
    phase: "edit",
    guardKinds: [],
    sending: false,
    error: null,
  });
}

/**
 * Tune a sender: open the rule editor prefilled with *@domain from the given
 * sender address (or the raw sender if no @). The editor POSTs /client/rules.
 */
function dispatchTune(sender: string): void {
  openRuleEditor(sender);
}

/** The verb surface the read views bind to keys. */
export interface Actions {
  archive: (u: AttentionUpdate) => void;
  done: (u: AttentionUpdate) => void;
  reopen: (u: AttentionUpdate) => void;
  reply: (u: AttentionUpdate) => void;
  tune: (sender: string) => void;
  label: (u: AttentionUpdate, add?: string[], remove?: string[]) => void;
}

/** Stable action object; safe to list in a useMemo/useKeys dep array. */
export function useActions(): Actions {
  return useMemo<Actions>(
    () => ({
      archive: (u) => void dispatchArchive(u),
      done: (u) => void dispatchDone(u),
      reopen: (u) => void dispatchReopen(u),
      reply: (u) => dispatchReply(u),
      tune: (sender) => dispatchTune(sender),
      label: (u, add, remove) => void dispatchLabel(u, add, remove),
    }),
    [],
  );
}

// Raw dispatchers for non-hook callers (process-mode reducers, tests).
export {
  dispatchArchive,
  dispatchDone,
  dispatchReopen,
  dispatchLabel,
  dispatchReply,
  dispatchTune,
};
