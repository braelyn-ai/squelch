// AUTH CODE MODAL — the payoff of the 2FA "present, don't read" flow. When an
// otp/login_code/verification message arrives we auto-reveal (audited) and
// extract the code; this modal presents it BIG so the human copies it and moves
// on without ever reading the email.
//
// Follows the canonical overlay contract: conditional-mount (parent renders only
// while authQueue is non-empty), its own "modal" KeyContext, Esc/Enter dismiss,
// backdrop click-to-close. The code lives in store state only (never persisted)
// and is dropped from the queue on dismiss. If multiple codes arrive they queue
// newest-first; dismissing advances to the next.

import { useMemo, useState, useEffect } from "react";
import { KeyRound, Copy, Check } from "lucide-react";
import { useStore } from "../state";
import { useKeys, useKeyContext } from "../keys";
import { authKindLabel, authKindIcon } from "../lib/authCopy";
import { senderDisplayName } from "../lib/avatar";
import { Avatar } from "./Avatar";
import { copyText } from "../lib/clipboard";

export function AuthCodeModal() {
  const entry = useStore((s) => s.authQueue[0]);
  const queueLen = useStore((s) => s.authQueue.length);
  const dismissAuthCode = useStore((s) => s.dismissAuthCode);
  const setView = useStore((s) => s.setView);
  const [copied, setCopied] = useState(false);

  // Reset the "copied" flash whenever we advance to a new queued code.
  const codeId = entry?.meta.id;
  useEffect(() => setCopied(false), [codeId]);

  const KindIcon = entry ? authKindIcon(entry.meta.kind) : KeyRound;
  const code = entry?.code ?? null;

  async function copy() {
    if (!code) return;
    if (await copyText(code)) {
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1400);
    }
  }

  function openAuth() {
    dismissAuthCode();
    setView("auth");
  }

  useKeyContext("modal");
  const bindings = useMemo(
    () => [
      { key: "Escape", description: "dismiss", handler: () => dismissAuthCode() },
      { key: "Enter", description: "dismiss", handler: () => dismissAuthCode() },
      { key: "c", description: "copy code", handler: () => void copy() },
    ],
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [code, dismissAuthCode],
  );
  useKeys("modal", bindings, [bindings]);

  if (!entry) return null;

  return (
    <div className="authcode-overlay" onClick={() => dismissAuthCode()}>
      <div className="authcode-card" onClick={(e) => e.stopPropagation()}>
        <div className="authcode-head">
          <Avatar sender={entry.meta.sender} size={28} />
          <div className="authcode-who">
            <span className="authcode-sender" title={entry.meta.sender}>
              {senderDisplayName(entry.meta.sender)}
            </span>
            <span className="authcode-kind">
              <KindIcon size={13} /> {authKindLabel(entry.meta.kind)}
            </span>
          </div>
          {queueLen > 1 && (
            <span className="authcode-queue" title="more codes waiting">
              +{queueLen - 1}
            </span>
          )}
        </div>

        {code ? (
          <div className="authcode-code" aria-label="login code">
            {code}
          </div>
        ) : (
          <div className="authcode-nocode">
            couldn't read a code from this one — open Auth to reveal it yourself.
          </div>
        )}

        <div className="authcode-actions">
          {code ? (
            <button
              type="button"
              className="authcode-btn primary"
              onClick={() => void copy()}
            >
              {copied ? <Check size={15} /> : <Copy size={15} />}{" "}
              {copied ? "copied" : "copy"} <kbd>c</kbd>
            </button>
          ) : (
            <button type="button" className="authcode-btn primary" onClick={openAuth}>
              <KeyRound size={15} /> open Auth
            </button>
          )}
          <button
            type="button"
            className="authcode-btn"
            onClick={() => dismissAuthCode()}
          >
            dismiss <kbd>esc</kbd>
          </button>
        </div>

        <div className="authcode-foot">
          not stored · revealing it was audited
        </div>
      </div>
    </div>
  );
}
