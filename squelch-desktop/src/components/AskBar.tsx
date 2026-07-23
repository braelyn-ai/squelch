// ⌘K "ASK YOUR INBOX" — the embedded assistant's command bar. A top-anchored
// overlay: type a question, get a cited answer, jump to a thread if you want the
// detail. The whole loop runs client-side with the user's own key (BYOK) through
// the Rust proxy; the squelch server never sees it.
//
// Follows the overlay contract: conditional-mount by the parent (ActionLayer),
// its own "modal" KeyContext, Esc + backdrop-click to close. Enter submits from
// the input directly (not via the keymap) so it never collides with list keys.

import { useEffect, useMemo, useRef, useState } from "react";
import { Sparkles, CornerDownLeft } from "lucide-react";
import { useStore } from "../state";
import { useKeys, useKeyContext } from "../keys";
import { askInbox, type AssistantAnswer } from "../api/assistant/agent";

type Phase = "idle" | "loading" | "done" | "error";

export function AskBar({ onClose }: { onClose: () => void }) {
  const [q, setQ] = useState("");
  const [phase, setPhase] = useState<Phase>("idle");
  const [answer, setAnswer] = useState<AssistantAnswer | null>(null);
  const [error, setError] = useState<string | null>(null);
  const openThread = useStore((s) => s.openThread);
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    inputRef.current?.focus();
  }, []);

  useKeyContext("modal");
  const bindings = useMemo(
    () => [
      {
        key: "Escape",
        allowInInput: true,
        description: "close",
        handler: () => onClose(),
      },
    ],
    [onClose],
  );
  useKeys("modal", bindings, [bindings]);

  async function submit() {
    const question = q.trim();
    if (!question || phase === "loading") return;
    setPhase("loading");
    setError(null);
    setAnswer(null);
    try {
      const a = await askInbox(question);
      setAnswer(a);
      setPhase("done");
    } catch (e) {
      setError(e instanceof Error ? e.message : "something went wrong");
      setPhase("error");
    }
  }

  function openCite(threadId: string) {
    openThread(threadId);
    onClose();
  }

  return (
    <div className="askbar-overlay" onClick={() => onClose()}>
      <div className="askbar" onClick={(e) => e.stopPropagation()}>
        <div className="askbar-head">
          <Sparkles size={13} strokeWidth={2} />
          <span className="askbar-label">ask your inbox</span>
          <span className="askbar-esc">
            <kbd>esc</kbd>
          </span>
        </div>

        <div className="askbar-inputrow">
          <input
            ref={inputRef}
            className="askbar-input"
            value={q}
            placeholder="e.g. what did I say I'd send Dan?"
            onChange={(e) => setQ(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") {
                e.preventDefault();
                void submit();
              }
            }}
            disabled={phase === "loading"}
          />
          <CornerDownLeft
            size={13}
            className={`askbar-enter ${q.trim() ? "ready" : ""}`}
          />
        </div>

        {phase === "loading" && (
          <div className="askbar-status">searching your inbox…</div>
        )}
        {phase === "error" && error && (
          <div className="askbar-error">{error}</div>
        )}
        {phase === "done" && answer && (
          <div className="askbar-answer">
            <p className="askbar-text">{answer.text}</p>
            {answer.citations.length > 0 && (
              <ul className="askbar-cites">
                {answer.citations.map((c) => (
                  <li key={c.threadId}>
                    <button
                      className="askbar-cite"
                      onClick={() => openCite(c.threadId)}
                      title="Open this thread"
                    >
                      <span className="askbar-cite-sender">{c.sender}</span>
                      <span className="askbar-cite-subject">{c.subject}</span>
                    </button>
                  </li>
                ))}
              </ul>
            )}
          </div>
        )}
      </div>
    </div>
  );
}
