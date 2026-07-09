// Single keymap registry. ALL keybindings in the app flow through here so they
// live in one system and can't collide silently. View agents register a set of
// bindings scoped to a KeyContext; the active context is a stack (last pushed
// wins) so a modal transparently captures keys until it pops.
//
// Design laws (UX-DIRECTIONS): keyboard-first. list context owns
// j/k/Enter/r/e/d/p/a/t/T/'/'/u/Esc; modal/input contexts override as needed.
// Typing into an <input>/<textarea> suppresses single-letter list bindings
// automatically (the "input" context guard) unless the binding opts in.

import { useEffect } from "react";

/** Where a binding is active. The registry tracks a stack of contexts. */
export type KeyContext = "list" | "modal" | "input" | "global";

export interface KeyBinding {
  /** Physical intent, e.g. "j", "Enter", "Escape", "/", "shift+T". */
  key: string;
  /** Human label for a future help overlay. */
  description: string;
  /** Return true if handled (stops propagation to lower contexts). */
  handler: (e: KeyboardEvent) => boolean | void;
  /** If true, fires even while an input/textarea is focused. Default false. */
  allowInInput?: boolean;
}

interface RegisteredSet {
  id: string;
  context: KeyContext;
  bindings: KeyBinding[];
}

// --- module-level registry (single source of truth) -------------------------

const sets: RegisteredSet[] = [];
// Context stack: the topmost non-empty context is the only one that receives
// keys (plus "global" which always sees them last). Views push/pop as they mount.
const contextStack: KeyContext[] = ["list"];

let seq = 0;

function normalize(e: KeyboardEvent): string {
  const parts: string[] = [];
  if (e.ctrlKey) parts.push("ctrl");
  if (e.metaKey) parts.push("meta");
  if (e.altKey) parts.push("alt");
  // Shift is encoded only for named keys; letters already arrive cased.
  let k = e.key;
  if (k === " ") k = "Space";
  if (e.shiftKey && k.length > 1) parts.push("shift");
  parts.push(k);
  return parts.join("+");
}

function keyMatches(binding: string, event: string): boolean {
  if (binding === event) return true;
  // Case-insensitive fallback for single letters where shift produced a cap.
  return binding.toLowerCase() === event.toLowerCase();
}

function isEditable(el: EventTarget | null): boolean {
  if (!(el instanceof HTMLElement)) return false;
  const tag = el.tagName;
  return (
    tag === "INPUT" ||
    tag === "TEXTAREA" ||
    tag === "SELECT" ||
    el.isContentEditable
  );
}

function activeContext(): KeyContext {
  return contextStack[contextStack.length - 1] ?? "list";
}

function dispatch(e: KeyboardEvent): void {
  const event = normalize(e);
  const editing = isEditable(e.target);
  const ctx = activeContext();

  // Contexts that get a shot, in priority order: active context, then global.
  const order: KeyContext[] = ctx === "global" ? ["global"] : [ctx, "global"];

  for (const context of order) {
    // Later registrations win within a context (a view mounted on top).
    for (let i = sets.length - 1; i >= 0; i--) {
      const s = sets[i];
      if (s.context !== context) continue;
      for (const b of s.bindings) {
        if (!keyMatches(b.key, event)) continue;
        if (editing && !b.allowInInput) continue;
        const handled = b.handler(e);
        if (handled !== false) {
          e.preventDefault();
          return;
        }
      }
    }
  }
}

// Install the single global listener exactly once.
let installed = false;
function ensureListener(): void {
  if (installed) return;
  installed = true;
  window.addEventListener("keydown", dispatch);
}

// --- public registry API ----------------------------------------------------

/** Push a context onto the stack; returns a popper. Used by modals/side views. */
export function pushContext(ctx: KeyContext): () => void {
  contextStack.push(ctx);
  return () => {
    const idx = contextStack.lastIndexOf(ctx);
    if (idx > 0) contextStack.splice(idx, 1);
  };
}

/** The context currently receiving keys. */
export function currentContext(): KeyContext {
  return activeContext();
}

/**
 * Register a set of bindings for a context. Returns an unregister fn. Prefer the
 * `useKeys` hook in components; this raw form exists for non-hook callers.
 */
export function registerKeys(
  context: KeyContext,
  bindings: KeyBinding[],
): () => void {
  ensureListener();
  const id = `set-${seq++}`;
  sets.push({ id, context, bindings });
  return () => {
    const idx = sets.findIndex((s) => s.id === id);
    if (idx >= 0) sets.splice(idx, 1);
  };
}

/** All currently-registered bindings (for a help overlay). */
export function allBindings(): { context: KeyContext; binding: KeyBinding }[] {
  return sets.flatMap((s) =>
    s.bindings.map((binding) => ({ context: s.context, binding })),
  );
}

// --- hooks ------------------------------------------------------------------

/**
 * Register bindings for the lifetime of a component. `bindings` should be
 * stable (memoize with useMemo) or the effect re-registers each render; that is
 * safe but wasteful. Re-registers whenever `deps` change.
 */
export function useKeys(
  context: KeyContext,
  bindings: KeyBinding[],
  deps: React.DependencyList = [],
): void {
  useEffect(() => {
    return registerKeys(context, bindings);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, deps);
}

/**
 * Push a KeyContext while a component (modal / side view) is mounted, so its
 * bindings capture keys above the list. Pair with useKeys(context, ...).
 */
export function useKeyContext(ctx: KeyContext): void {
  useEffect(() => pushContext(ctx), [ctx]);
}
