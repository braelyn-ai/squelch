// Open bus for the ⌘K "ask your inbox" bar. Same pattern as ruleEditorBus: a
// tiny module-level pub/sub so the global ⌘K binding (App) can trigger the
// overlay that ActionLayer owns and renders, without a store slice.

type Listener = () => void;

const listeners = new Set<Listener>();

/** Open the ask bar (⌘K). */
export function openAskBar(): void {
  for (const l of listeners) l();
}

export function onOpenAskBar(l: Listener): () => void {
  listeners.add(l);
  return () => listeners.delete(l);
}
