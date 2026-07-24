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

import { useMemo, useState, useEffect, useRef } from "react";
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

  // AUTO-DISMISS after 30s, with a visible countdown: a code you haven't
  // grabbed in half a minute is a code you're not grabbing — and a secret
  // shouldn't sit on screen indefinitely. The timer is PER CODE (advancing the
  // queue restarts it) and copying PAUSES it: you clearly still want the code
  // (some flows need it pasted more than once), so it stays until you dismiss.
  const AUTO_DISMISS_S = 30;
  const [secondsLeft, setSecondsLeft] = useState(AUTO_DISMISS_S);
  const pausedRef = useRef(false);
  useEffect(() => {
    if (!codeId) return;
    pausedRef.current = false;
    setSecondsLeft(AUTO_DISMISS_S);
    const iv = window.setInterval(() => {
      if (pausedRef.current) return;
      setSecondsLeft((s) => s - 1);
    }, 1000);
    return () => window.clearInterval(iv);
  }, [codeId]);
  useEffect(() => {
    if (secondsLeft <= 0) dismissAuthCode();
  }, [secondsLeft, dismissAuthCode]);

  const KindIcon = entry ? authKindIcon(entry.meta.kind) : KeyRound;
  const code = entry?.code ?? null;

  async function copy() {
    if (!code) return;
    if (await copyText(code)) {
      setCopied(true);
      // You grabbed it — stop the countdown; dismissal is yours now.
      pausedRef.current = true;
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
          {!pausedRef.current && secondsLeft > 0 && (
            <span
              className="authcode-timer num"
              title="auto-dismisses; copying keeps it open"
            >
              · {secondsLeft}s
            </span>
          )}
        </div>
        {/* Slim draining countdown bar along the card's bottom edge. Freezes
            (via the paused class) once the code has been copied. */}
        <div
          className={`authcode-timerbar${pausedRef.current ? " paused" : ""}`}
          style={{ width: `${(Math.max(0, secondsLeft) / AUTO_DISMISS_S) * 100}%` }}
          aria-hidden="true"
        />
      </div>
    </div>
  );
}
