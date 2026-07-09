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
import { activeContext, dispatchCore, isEditableTag } from "./dispatchCore";
import type {
  KeyBinding,
  KeyContext,
  RegisteredSet,
} from "./dispatchCore";

export type { KeyBinding, KeyContext } from "./dispatchCore";

// --- module-level registry (single source of truth) -------------------------

const sets: RegisteredSet[] = [];
// Context stack: the topmost non-empty context is the only one that receives
// keys (plus "global" which always sees them last). Views push/pop as they mount.
const contextStack: KeyContext[] = ["list"];

let seq = 0;

function isEditable(el: EventTarget | null): boolean {
  if (!(el instanceof HTMLElement)) return false;
  return isEditableTag(el.tagName, el.isContentEditable);
}

/**
 * The real window keydown handler: reads DOM focus state, then delegates all
 * matching/priority logic to the pure `dispatchCore` so the exact same code the
 * app runs is what the unit tests exercise. Calls preventDefault when handled.
 */
function dispatch(e: KeyboardEvent): void {
  const result = dispatchCore({
    sets,
    contextStack,
    event: e,
    editing: isEditable(e.target),
  });
  if (result.handled) e.preventDefault();
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
  return activeContext(contextStack);
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
