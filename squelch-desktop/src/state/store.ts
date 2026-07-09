// The single app store (zustand). One store, several logical slices:
//   settings   — connection state + Connect flow
//   sitrep     — the three bands, stats, sealed metadata (the read model)
//   selection  — cursor position, stable by message id across refresh
//   undo       — pending-undo queue for undo-first actions
//   sideView   — which side view (thread/rules/browse/search) is mounted
//
// The store holds DATA and coordination only. Network calls live in src/api and
// are invoked by hooks (useSitrep) / view agents, which then write results here.

import { create } from "zustand";
import { api } from "../api";
import { ApiError } from "../api";
import type {
  AttentionUpdate,
  SealedMeta,
  StoreStats,
} from "../api";
import { configureClient } from "../api/client";
import { getSettings, setSettings, type Settings } from "../api/settings";

// --- band model -------------------------------------------------------------

export type BandKey = "standing" | "new" | "open";

/** The read model the SitrepView renders: updates bucketed by band. */
export interface SitrepData {
  standing: AttentionUpdate[];
  new: AttentionUpdate[];
  open: AttentionUpdate[];
  stats: StoreStats | null;
  sealed: SealedMeta[];
}

// --- undo model -------------------------------------------------------------

export type UndoKind = "archive" | "done" | "label" | "rule_delete";

/**
 * A queued undo. `revert` is the exact inverse call to fire on `u`/toast-click.
 * `expiresAt` drives the 5s auto-expiry. Undo-first design: the forward action
 * already fired; this lets the human take it back.
 */
export interface PendingUndo {
  id: string;
  kind: UndoKind;
  /** The message id for mail actions; the (now-deleted) rule id for rule_delete. */
  messageId: number;
  label: string; // human text for the toast, e.g. "archived PG&E"
  createdAt: number;
  expiresAt: number;
  revert: () => Promise<void>;
}

// --- connection state -------------------------------------------------------

export type ConnStatus =
  | "loading" // reading keyring on boot
  | "disconnected" // no settings -> Connect screen
  | "connecting" // testing a candidate URL+token
  | "connected"
  | "error";

// --- transient side views ---------------------------------------------------

export type SideView =
  | { kind: "none" }
  | { kind: "thread"; threadId: string }
  | { kind: "rules" }
  | { kind: "browse" }
  | { kind: "search"; query: string }
  | { kind: "audit" }
  | { kind: "auth" };

// --- toast (non-undo, ephemeral notices) ------------------------------------

export interface Toast {
  id: string;
  text: string;
  tone: "info" | "error" | "success";
}

export interface AppState {
  // settings slice
  connStatus: ConnStatus;
  settings: Settings | null;
  connError: string | null;
  loadSettings: () => Promise<void>;
  /** Test a candidate URL+token via /client/stats; on success persist + connect. */
  connect: (serverUrl: string, apiToken: string) => Promise<boolean>;
  disconnect: () => void;

  // sitrep slice
  sitrep: SitrepData;
  lastRefresh: number | null;
  refreshError: string | null;
  setSitrep: (partial: Partial<SitrepData>) => void;
  setRefreshError: (msg: string | null) => void;
  markRefreshed: () => void;

  // selection slice — stable by message id
  selectedId: number | null;
  /** Flat, band-ordered id list the keymap uses for j/k movement. */
  orderedIds: () => number[];
  select: (id: number | null) => void;
  moveSelection: (delta: 1 | -1) => void;
  selectedUpdate: () => AttentionUpdate | null;

  // undo slice
  undos: PendingUndo[];
  pushUndo: (u: Omit<PendingUndo, "id" | "createdAt" | "expiresAt">) => string;
  fireUndo: (id?: string) => Promise<void>; // undo the given (or most recent)
  expireUndo: (id: string) => void;

  // toasts
  toasts: Toast[];
  pushToast: (text: string, tone?: Toast["tone"]) => string;
  dismissToast: (id: string) => void;

  // side view slice
  sideView: SideView;
  openSide: (view: SideView) => void;
  closeSide: () => void;

  // compose slice (send ceremony lives in ActionLayer; state is shared here)
  compose: ComposeState | null;
  openCompose: (init: Partial<ComposeState>) => void;
  updateCompose: (patch: Partial<ComposeState>) => void;
  closeCompose: () => void;
}

/** Draft + review state for the send ceremony. */
export interface ComposeState {
  replyToMessageId?: number;
  to: string;
  subject: string;
  body: string;
  /** "edit" = composing; "review" = guard verdict shown, second Enter fires. */
  phase: "edit" | "review";
  /** Redacted guard kinds from a 422; empty means guard passed. */
  guardKinds: string[];
  sending: boolean;
  error: string | null;
}

const UNDO_TTL_MS = 5000;

function uid(): string {
  return Math.random().toString(36).slice(2, 10);
}

const emptySitrep: SitrepData = {
  standing: [],
  new: [],
  open: [],
  stats: null,
  sealed: [],
};

export const useStore = create<AppState>((set, get) => ({
  // --- settings -------------------------------------------------------------
  connStatus: "loading",
  settings: null,
  connError: null,

  loadSettings: async () => {
    try {
      const s = await getSettings();
      if (s && s.server_url && s.api_token) {
        configureClient(s.server_url, s.api_token);
        set({ settings: s, connStatus: "connected", connError: null });
      } else {
        set({ connStatus: "disconnected" });
      }
    } catch (e) {
      set({
        connStatus: "disconnected",
        connError: e instanceof Error ? e.message : "settings load failed",
      });
    }
  },

  connect: async (serverUrl, apiToken) => {
    set({ connStatus: "connecting", connError: null });
    // Probe with a throwaway config so a bad token never gets persisted.
    configureClient(serverUrl, apiToken);
    try {
      await api.getStats(); // 401 here => bad token; network => bad url
      await setSettings({ server_url: serverUrl, api_token: apiToken });
      set({
        settings: { server_url: serverUrl, api_token: apiToken },
        connStatus: "connected",
        connError: null,
      });
      return true;
    } catch (e) {
      const msg =
        e instanceof ApiError
          ? e.kind === "unauthorized"
            ? "token rejected (401)"
            : e.kind === "network"
              ? "cannot reach that server URL"
              : e.message
          : "connection failed";
      set({ connStatus: "error", connError: msg });
      return false;
    }
  },

  disconnect: () => {
    set({
      connStatus: "disconnected",
      settings: null,
      sitrep: emptySitrep,
      selectedId: null,
    });
  },

  // --- sitrep ---------------------------------------------------------------
  sitrep: emptySitrep,
  lastRefresh: null,
  refreshError: null,
  setSitrep: (partial) =>
    set((s) => ({ sitrep: { ...s.sitrep, ...partial } })),
  setRefreshError: (msg) => set({ refreshError: msg }),
  markRefreshed: () => set({ lastRefresh: Date.now() }),

  // --- selection ------------------------------------------------------------
  selectedId: null,
  orderedIds: () => {
    const { standing, new: fresh, open } = get().sitrep;
    return [...standing, ...fresh, ...open].map((u) => u.id);
  },
  select: (id) => set({ selectedId: id }),
  moveSelection: (delta) => {
    const ids = get().orderedIds();
    if (ids.length === 0) return;
    const cur = get().selectedId;
    const idx = cur === null ? -1 : ids.indexOf(cur);
    let next = idx + delta;
    if (next < 0) next = 0;
    if (next > ids.length - 1) next = ids.length - 1;
    set({ selectedId: ids[next] });
  },
  selectedUpdate: () => {
    const id = get().selectedId;
    if (id === null) return null;
    const { standing, new: fresh, open } = get().sitrep;
    return (
      [...standing, ...fresh, ...open].find((u) => u.id === id) ?? null
    );
  },

  // --- undo -----------------------------------------------------------------
  undos: [],
  pushUndo: (u) => {
    const id = uid();
    const now = Date.now();
    const entry: PendingUndo = {
      ...u,
      id,
      createdAt: now,
      expiresAt: now + UNDO_TTL_MS,
    };
    set((s) => ({ undos: [...s.undos, entry] }));
    // Auto-expire from the queue after the window.
    setTimeout(() => get().expireUndo(id), UNDO_TTL_MS);
    return id;
  },
  fireUndo: async (id) => {
    const list = get().undos;
    const entry = id
      ? list.find((u) => u.id === id)
      : list[list.length - 1];
    if (!entry) return;
    set((s) => ({ undos: s.undos.filter((u) => u.id !== entry.id) }));
    try {
      await entry.revert();
      get().pushToast(`undone: ${entry.label}`, "info");
    } catch {
      get().pushToast(`undo failed: ${entry.label}`, "error");
    }
  },
  expireUndo: (id) =>
    set((s) => ({ undos: s.undos.filter((u) => u.id !== id) })),

  // --- toasts ---------------------------------------------------------------
  toasts: [],
  pushToast: (text, tone = "info") => {
    const id = uid();
    set((s) => ({ toasts: [...s.toasts, { id, text, tone }] }));
    return id;
  },
  dismissToast: (id) =>
    set((s) => ({ toasts: s.toasts.filter((t) => t.id !== id) })),

  // --- side view ------------------------------------------------------------
  sideView: { kind: "none" },
  openSide: (view) => set({ sideView: view }),
  closeSide: () => set({ sideView: { kind: "none" } }),

  // --- compose --------------------------------------------------------------
  compose: null,
  openCompose: (init) =>
    set({
      compose: {
        to: "",
        subject: "",
        body: "",
        phase: "edit",
        guardKinds: [],
        sending: false,
        error: null,
        ...init,
      },
    }),
  updateCompose: (patch) =>
    set((s) => (s.compose ? { compose: { ...s.compose, ...patch } } : s)),
  closeCompose: () => set({ compose: null }),
}));
