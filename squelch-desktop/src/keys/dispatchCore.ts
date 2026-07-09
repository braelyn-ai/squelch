// Pure dispatch core for the keymap registry. No React, no DOM globals — this is
// the logic useKeys.ts wires a real `window` keydown listener into. Keeping it
// pure lets it be unit-tested directly (see dispatchCore.test.ts) which is how we
// proved the "modal context permanently on the stack" gating bug and its fix.

export type KeyContext = "list" | "modal" | "input" | "global";

export interface KeyBinding {
  key: string;
  description: string;
  handler: (e: KeyboardEvent) => boolean | void;
  allowInInput?: boolean;
}

export interface RegisteredSet {
  id: string;
  context: KeyContext;
  bindings: KeyBinding[];
}

/** A minimal view of the keyboard event the pure core needs. */
export interface KeyEventLike {
  key: string;
  ctrlKey?: boolean;
  metaKey?: boolean;
  altKey?: boolean;
  shiftKey?: boolean;
}

export function normalize(e: KeyEventLike): string {
  const parts: string[] = [];
  if (e.ctrlKey) parts.push("ctrl");
  if (e.metaKey) parts.push("meta");
  if (e.altKey) parts.push("alt");
  let k = e.key;
  if (k === " ") k = "Space";
  if (e.shiftKey && k.length > 1) parts.push("shift");
  parts.push(k);
  return parts.join("+");
}

export function keyMatches(binding: string, event: string): boolean {
  if (binding === event) return true;
  return binding.toLowerCase() === event.toLowerCase();
}

export function activeContext(contextStack: KeyContext[]): KeyContext {
  return contextStack[contextStack.length - 1] ?? "list";
}

/** Pure editable-target test (DOM-free): given a tag name + contentEditable. */
export function isEditableTag(tagName: string, isContentEditable: boolean): boolean {
  return (
    tagName === "INPUT" ||
    tagName === "TEXTAREA" ||
    tagName === "SELECT" ||
    isContentEditable
  );
}

export interface DispatchInput {
  sets: RegisteredSet[];
  contextStack: KeyContext[];
  /** Normalized key string (from `normalize`) or a raw KeyEventLike. */
  event: KeyEventLike;
  /** Whether focus is in an editable element (input/textarea/etc.). */
  editing: boolean;
}

export interface DispatchResult {
  /** True if some binding claimed the key (handler returned !== false). */
  handled: boolean;
  /** The binding that fired, for assertions/debugging. null if none. */
  firedKey: string | null;
  firedContext: KeyContext | null;
}

/**
 * The pure dispatch algorithm. Walks the active context (then global), latest
 * registration first, and fires the first matching, non-input-suppressed
 * binding. Returns what happened. The real listener calls e.preventDefault()
 * when `handled` is true.
 */
export function dispatchCore(input: DispatchInput): DispatchResult {
  const { sets, contextStack, editing } = input;
  const eventStr = normalize(input.event);
  const ctx = activeContext(contextStack);
  const order: KeyContext[] = ctx === "global" ? ["global"] : [ctx, "global"];

  for (const context of order) {
    for (let i = sets.length - 1; i >= 0; i--) {
      const s = sets[i];
      if (s.context !== context) continue;
      for (const b of s.bindings) {
        if (!keyMatches(b.key, eventStr)) continue;
        if (editing && !b.allowInInput) continue;
        const handled = b.handler(input.event as unknown as KeyboardEvent);
        if (handled !== false) {
          return { handled: true, firedKey: b.key, firedContext: context };
        }
      }
    }
  }
  return { handled: false, firedKey: null, firedContext: null };
}
