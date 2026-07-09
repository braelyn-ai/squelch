// Overlay-open bus for the two ActionLayer-owned modals that have no store slice
// of their own (the store is scaffold-owned and I don't extend it): the rule
// editor (`t` tune) and process mode (`p`). A tiny module-level pub/sub lets the
// read views trigger these overlays while ActionLayer owns their state/render.
//
// This is deliberately not in the zustand store: these are transient, action-
// side overlays that only ActionLayer mounts.

type Listener<T> = (payload: T) => void;

function bus<T>() {
  const listeners = new Set<Listener<T>>();
  return {
    emit(payload: T) {
      for (const l of listeners) l(payload);
    },
    subscribe(l: Listener<T>): () => void {
      listeners.add(l);
      return () => listeners.delete(l);
    },
  };
}

const ruleEditor = bus<{ sender: string }>();
const processMode = bus<void>();

/** Open the rule editor prefilled from a sender address (read views call this). */
export function openRuleEditor(sender: string): void {
  ruleEditor.emit({ sender });
}
export function onOpenRuleEditor(l: (p: { sender: string }) => void): () => void {
  return ruleEditor.subscribe(l);
}

/** Enter process mode — card-by-card walk of NEW + STILL OPEN. */
export function openProcessMode(): void {
  processMode.emit();
}
export function onOpenProcessMode(l: () => void): () => void {
  return processMode.subscribe(l);
}
