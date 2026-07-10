// SETTINGS — a routed main view (bottom rail group). Precision-instrument
// styled: engraved section labels, brass used sparingly (focus + the connected
// dot). Sections:
//   CONNECTION — server URL + API token (masked), "Test & Save" re-validates
//                against /client/stats via the store's connect() and persists.
//   APPEARANCE — light/dark theme, a real control mirroring the '\' toggle.
//   ACCOUNT    — read-only: server URL + triage model/provider (from /client/usage).
//   DISCONNECT — clears saved settings and returns to the Connect gate.
//
// Keyboard: no keys are registered here, so the global 1..5 / cmd+[ ] nav keeps
// working and Esc is a no-op (nothing to close). The dispatchCore input-guard
// already prevents single-letter binds from firing while a field is focused.

import { useEffect, useState } from "react";
import { useStore } from "../state";
import { api } from "../api";
import type { UsageResponse } from "../api";
import { setThemeTo, subscribeTheme } from "../components/ThemeToggle";
import { currentTheme, type Theme } from "../state/theme";
import "../styles/settings.css";

export function SettingsView() {
  const settings = useStore((s) => s.settings);
  const revalidate = useStore((s) => s.revalidate);
  const disconnect = useStore((s) => s.disconnect);

  // CONNECTION form — seeded from the live settings. Test/Save state is LOCAL so
  // a bad token never bounces the whole app back to the Connect gate.
  const [url, setUrl] = useState(settings?.server_url ?? "");
  const [token, setToken] = useState(settings?.api_token ?? "");
  const [busy, setBusy] = useState(false);
  const [result, setResult] = useState<
    { ok: true } | { ok: false; error: string } | null
  >(null);

  // APPEARANCE — reflects the document theme, kept in sync with the '\' toggle.
  const [theme, setTheme] = useState<Theme>(() => currentTheme());
  useEffect(() => subscribeTheme(setTheme), []);

  // ACCOUNT — triage model/provider label, best-effort from /client/usage.
  const [usage, setUsage] = useState<UsageResponse | null>(null);
  useEffect(() => {
    let alive = true;
    api
      .getUsage(1)
      .then((u) => alive && setUsage(u))
      .catch(() => {
        /* usage is decorative here; ignore errors */
      });
    return () => {
      alive = false;
    };
  }, []);

  async function onTestSave(e: React.FormEvent) {
    e.preventDefault();
    setResult(null);
    if (!url.trim() || !token.trim()) return;
    setBusy(true);
    const r = await revalidate(url.trim(), token.trim());
    setBusy(false);
    setResult(r.ok ? { ok: true } : { ok: false, error: r.error ?? "failed" });
  }

  return (
    <div className="routed-view">
      <header className="routed-head">
        <h2>Settings</h2>
      </header>
      <div className="routed-body settings">
        {/* CONNECTION ---------------------------------------------------- */}
        <section className="set-section">
          <div className="set-label">Connection</div>
          <form className="set-form" onSubmit={onTestSave}>
            <div className="set-field">
              <label htmlFor="set-url">server url</label>
              <input
                id="set-url"
                value={url}
                onChange={(e) => {
                  setUrl(e.target.value);
                  setResult(null);
                }}
                placeholder="http://127.0.0.1:8848"
                autoComplete="off"
                spellCheck={false}
              />
            </div>
            <div className="set-field">
              <label htmlFor="set-token">api token</label>
              <input
                id="set-token"
                type="password"
                value={token}
                onChange={(e) => {
                  setToken(e.target.value);
                  setResult(null);
                }}
                placeholder="SQUELCH_API_TOKEN"
                autoComplete="off"
                spellCheck={false}
              />
            </div>
            <div className="set-row">
              <button type="submit" className="primary" disabled={busy}>
                {busy ? "testing…" : "Test & Save"}
              </button>
              {result?.ok && (
                <span className="set-status ok">
                  <span className="dot" /> connected · saved
                </span>
              )}
              {result && !result.ok && (
                <span className="set-status err">{result.error}</span>
              )}
            </div>
          </form>
        </section>

        {/* APPEARANCE ---------------------------------------------------- */}
        <section className="set-section">
          <div className="set-label">Appearance</div>
          <div className="set-field-inline">
            <span className="set-key">theme</span>
            <div className="set-toggle" role="group" aria-label="theme">
              <button
                type="button"
                className={theme === "light" ? "active" : ""}
                aria-pressed={theme === "light"}
                onClick={() => setThemeTo("light")}
              >
                Light
              </button>
              <button
                type="button"
                className={theme === "dark" ? "active" : ""}
                aria-pressed={theme === "dark"}
                onClick={() => setThemeTo("dark")}
              >
                Dark
              </button>
            </div>
          </div>
        </section>

        {/* ACCOUNT ------------------------------------------------------- */}
        <section className="set-section">
          <div className="set-label">Account</div>
          <dl className="set-meta">
            <div>
              <dt>server</dt>
              <dd className="mono">{settings?.server_url ?? "—"}</dd>
            </div>
            <div>
              <dt>triage model</dt>
              <dd className="mono">{usage?.model ?? "—"}</dd>
            </div>
            <div>
              <dt>provider</dt>
              <dd className="mono">{usage?.provider ?? "—"}</dd>
            </div>
          </dl>
        </section>

        {/* DISCONNECT ---------------------------------------------------- */}
        <section className="set-section">
          <div className="set-label">Danger</div>
          <div className="set-row">
            <button
              type="button"
              className="danger"
              onClick={() => disconnect()}
            >
              Disconnect
            </button>
            <span className="set-hint">
              clears saved settings and returns to the connect screen
            </span>
          </div>
        </section>
      </div>
    </div>
  );
}
