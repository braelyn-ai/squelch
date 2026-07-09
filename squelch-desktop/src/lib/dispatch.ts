// ACTION DISPATCH SEAM — the single seam between the read side (SitrepView and
// the other read views) and the write/action layer (actions/useActions, owned by
// view-agent-2). This module is intentionally a thin re-export: the read views
// import stable verb names from here, and the REAL implementations live in
// actions/useActions.ts (optimistic band removal, undo-first toasts, the compose
// ceremony, forbidden-credential handling). Previously this file carried its own
// parallel reimplementations that drifted from the action layer (no optimistic
// removal, no 403 toast, a thinner compose payload); it now delegates so there is
// exactly one action code path.
//
// SECURITY: never logs tokens or bodies. Errors surface as toasts only.

import type { AttentionUpdate } from "../api";
import {
  dispatchArchive,
  dispatchDone,
  dispatchReply,
  dispatchTune,
} from "../actions/useActions";

export { dispatchArchive, dispatchDone, dispatchReply };

/**
 * t — tune sender. The read views call this with the focused update; the real
 * action opens the rule editor prefilled from the sender address (the actual
 * sender-tuning surface), so adapt the signature here.
 */
export function dispatchTuneSender(u: AttentionUpdate): void {
  dispatchTune(u.sender);
}
