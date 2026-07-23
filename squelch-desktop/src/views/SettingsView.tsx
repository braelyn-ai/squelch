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
import { api, ApiError } from "../api";
import type {
  TriageConfig,
  TriageConfigPatch,
  TriageConfigSource,
  UsageResponse,
} from "../api";
import { setThemeTo, subscribeTheme } from "../components/ThemeToggle";
import { currentTheme, type Theme } from "../state/theme";
import {
  assistantKeyStatus,
  setAssistantKey,
  clearAssistantKey,
  type AssistantKeyStatus,
} from "../api/assistant/llm";
import {
  ASSISTANT_MODELS,
  getAssistantModel,
  setAssistantModel,
  type AssistantModelId,
} from "../api/assistant/agent";
import { TriagePipelineSection } from "../components/TriagePipeline";
import { getUserName, setUserName } from "../lib/identity";
import { usePref, setPref, type SettingsSection } from "../lib/prefs";
import "../styles/settings.css";

/** The left sub-nav: id (persisted pref) → visible label. Order = display order. */
const SETTINGS_SECTIONS: { id: SettingsSection; label: string }[] = [
  { id: "general", label: "General" },
  { id: "mail", label: "Mail" },
  { id: "triage", label: "Triage" },
  { id: "assistant", label: "Assistant" },
  { id: "account", label: "Account" },
];

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

  // YOU — display name for the Sitrep greeting (persisted on change).
  const [name, setName] = useState(() => getUserName());

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

  // Which sub-nav section is showing — persisted so reopening Settings restores
  // it. All section state lives on this component, so switching is instant and
  // in-flight form state (Test & Save, budget edits) survives a nav away/back.
  const section = usePref("settingsSection");

  return (
    <div className="routed-view">
      <header className="routed-head">
        <h2>Settings</h2>
      </header>
      <div className="routed-body settings">
        <nav className="set-nav" aria-label="settings sections">
          {SETTINGS_SECTIONS.map((s) => (
            <button
              key={s.id}
              type="button"
              className={`set-nav-item${section === s.id ? " active" : ""}`}
              aria-current={section === s.id ? "page" : undefined}
              onClick={() => setPref("settingsSection", s.id)}
            >
              {s.label}
            </button>
          ))}
        </nav>

        <div className="set-panel">
          {/* GENERAL — connection + appearance + you --------------------- */}
          {section === "general" && (
            <>
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

              <section className="set-section">
                <div className="set-label">Developer</div>
                <DeveloperSection />
              </section>

              <section className="set-section">
                <div className="set-label">You</div>
                <div className="set-field">
                  <label htmlFor="set-name">name</label>
                  <input
                    id="set-name"
                    value={name}
                    onChange={(e) => {
                      setName(e.target.value);
                      setUserName(e.target.value);
                    }}
                    placeholder="shown in the sitrep greeting"
                    autoComplete="off"
                    spellCheck={false}
                  />
                </div>
              </section>
            </>
          )}

          {/* MAIL — remote images --------------------------------------- */}
          {section === "mail" && <MailSection />}

          {/* TRIAGE — pipeline diagram + budget + ranking blend --------- */}
          {section === "triage" && (
            <>
              <TriagePipelineSection />
              <TriageBudgetSection />
              <RankingSection />
            </>
          )}

          {/* ASSISTANT — BYOK key + model ------------------------------- */}
          {section === "assistant" && <AssistantSettings />}

          {/* ACCOUNT — read-only meta + disconnect (danger) ------------- */}
          {section === "account" && (
            <>
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
            </>
          )}
        </div>
      </div>
    </div>
  );
}

/**
 * ASSISTANT — the BYOK "ask your inbox" (⌘K) settings: paste your OWN
 * Anthropic/OpenAI key (stored in the OS keyring, never sent to the squelch
 * server) and pick the model. The key is write-only: we can see whether one is
 * set and which provider it routes to, never the value itself.
 */
function AssistantSettings() {
  const [status, setStatus] = useState<AssistantKeyStatus | null>(null);
  const [keyInput, setKeyInput] = useState("");
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState<
    { ok: true; text: string } | { ok: false; text: string } | null
  >(null);
  const [model, setModel] = useState<AssistantModelId>(() => getAssistantModel());

  async function refresh() {
    try {
      setStatus(await assistantKeyStatus());
    } catch {
      setStatus({ present: false, provider: null });
    }
  }
  useEffect(() => {
    void refresh();
  }, []);

  async function onSaveKey(e: React.FormEvent) {
    e.preventDefault();
    const key = keyInput.trim();
    if (!key) return;
    setBusy(true);
    setNote(null);
    try {
      await setAssistantKey(key);
      setKeyInput("");
      await refresh();
      setNote({ ok: true, text: "key saved" });
    } catch (err) {
      setNote({
        ok: false,
        text: err instanceof Error ? err.message : "could not save key",
      });
    } finally {
      setBusy(false);
    }
  }

  async function onForget() {
    setBusy(true);
    setNote(null);
    try {
      await clearAssistantKey();
      await refresh();
      setNote({ ok: true, text: "key forgotten" });
    } finally {
      setBusy(false);
    }
  }

  function pickModel(id: AssistantModelId) {
    setModel(id);
    setAssistantModel(id);
  }

  const providerLabel =
    status?.provider === "anthropic"
      ? "Anthropic"
      : status?.provider === "openai"
        ? "OpenAI"
        : null;

  return (
    <section className="set-section">
      <div className="set-label">Assistant</div>

      <form className="set-form" onSubmit={onSaveKey}>
        <div className="set-field">
          <label htmlFor="set-asst-key">api key (yours)</label>
          <input
            id="set-asst-key"
            type="password"
            value={keyInput}
            onChange={(e) => {
              setKeyInput(e.target.value);
              setNote(null);
            }}
            placeholder={
              status?.present ? "•••••• set — paste to replace" : "sk-ant-…"
            }
            autoComplete="off"
            spellCheck={false}
          />
        </div>
        <div className="set-row">
          <button type="submit" className="primary" disabled={busy}>
            {busy ? "saving…" : "Save key"}
          </button>
          {status?.present && (
            <span className="set-status ok">
              <span className="dot" /> key set{providerLabel ? ` · ${providerLabel}` : ""}
            </span>
          )}
          {status?.present && (
            <button type="button" className="link" onClick={() => void onForget()}>
              forget
            </button>
          )}
          {note && (
            <span className={`set-status ${note.ok ? "ok" : "err"}`}>
              {note.text}
            </span>
          )}
        </div>
        <span className="set-hint">
          Your key stays on this machine (OS keyring) and is used only for the ⌘K
          assistant — never sent to the squelch server.
        </span>
      </form>

      <div className="set-field-inline" style={{ marginTop: 14 }}>
        <span className="set-key">model</span>
        <div className="set-toggle" role="group" aria-label="assistant model">
          {ASSISTANT_MODELS.map((m) => (
            <button
              key={m.id}
              type="button"
              className={model === m.id ? "active" : ""}
              aria-pressed={model === m.id}
              onClick={() => pickModel(m.id)}
              title={m.label}
            >
              {m.id === "claude-haiku-4-5" ? "Haiku" : "Opus"}
            </button>
          ))}
        </div>
      </div>
    </section>
  );
}

/**
 * TRIAGE BUDGET — the daily caps for the two triage stages plus a spend estimator
 * computed from the trailing-14d usage + configured prices. Stage 1 (a small LLM)
 * runs on EVERY email and has one cap: its global/day ceiling. Stage 2 (a capable
 * LLM) runs only on escalations and keeps the three per-thread / per-sender /
 * global caps. Caps are integers 1..=100000; Save posts ONLY changed fields.
 * Values can originate from the built-in default, the server config file, or a
 * live app override — the per-field note reflects that provenance.
 *
 * Self-contained: this section owns all of its own state and helpers. The
 * pipeline diagram (added by a later docs pass) can be dropped in without
 * touching the budget logic here.
 */
type Stage2CapKey = "thread_daily_cap" | "sender_daily_cap" | "global_daily_cap";
/** Form/patch keys: the three Stage-2 caps plus the Stage-1 global cap. */
type FormKey = Stage2CapKey | "stage1_global_daily_cap";

const STAGE2_CAP_FIELDS: { key: Stage2CapKey; label: string }[] = [
  { key: "thread_daily_cap", label: "per thread / day" },
  { key: "sender_daily_cap", label: "per sender / day" },
  { key: "global_daily_cap", label: "global / day" },
];

const ALL_FORM_KEYS: FormKey[] = [
  "stage1_global_daily_cap",
  ...STAGE2_CAP_FIELDS.map((f) => f.key),
];

/** The live cap value for a form key (Stage-1 lives under `cfg.stage1`). */
function capValue(cfg: TriageConfig, key: FormKey): number {
  return key === "stage1_global_daily_cap"
    ? cfg.stage1.global_daily_cap
    : cfg[key];
}

/** The provenance of a form key's value (Stage-1 has its own `source`). */
function capSource(cfg: TriageConfig, key: FormKey): TriageConfigSource {
  return key === "stage1_global_daily_cap"
    ? cfg.stage1.source
    : cfg.sources[key];
}

function sourceNote(source: TriageConfigSource): string {
  switch (source) {
    case "config":
      return "(from config file)";
    case "override":
      return "(app override)";
    default:
      return "(default)";
  }
}

/** Dollars: 2 decimals, but 3 when under a cent so tiny per-day figures survive. */
function fmtUsd(n: number): string {
  return `$${n.toFixed(n < 0.01 ? 3 : 2)}`;
}

const EMPTY_FORM: Record<FormKey, string> = {
  stage1_global_daily_cap: "",
  thread_daily_cap: "",
  sender_daily_cap: "",
  global_daily_cap: "",
};

function TriageBudgetSection() {
  const [cfg, setCfg] = useState<TriageConfig | null>(null);
  const [loadErr, setLoadErr] = useState<string | null>(null);
  // Local field text, seeded from the loaded config and re-seeded after a save.
  const [form, setForm] = useState<Record<FormKey, string>>(EMPTY_FORM);
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState<
    { ok: true } | { ok: false; error: string } | null
  >(null);

  function seed(c: TriageConfig) {
    setCfg(c);
    setForm({
      stage1_global_daily_cap: String(c.stage1.global_daily_cap),
      thread_daily_cap: String(c.thread_daily_cap),
      sender_daily_cap: String(c.sender_daily_cap),
      global_daily_cap: String(c.global_daily_cap),
    });
  }

  useEffect(() => {
    let alive = true;
    api
      .getTriageConfig()
      .then((c) => alive && seed(c))
      .catch((e) => {
        if (alive)
          setLoadErr(e instanceof ApiError ? e.message : "could not load");
      });
    return () => {
      alive = false;
    };
  }, []);

  async function onSave(e: React.FormEvent) {
    e.preventDefault();
    if (!cfg) return;
    // Post only fields whose parsed integer differs from the loaded value.
    const patch: TriageConfigPatch = {};
    for (const key of ALL_FORM_KEYS) {
      const raw = form[key].trim();
      const n = Number(raw);
      if (raw === "" || !Number.isInteger(n)) {
        setNote({ ok: false, error: "caps must be whole numbers" });
        return;
      }
      if (n !== capValue(cfg, key)) patch[key] = n;
    }
    if (Object.keys(patch).length === 0) {
      setNote({ ok: true });
      return;
    }
    setBusy(true);
    setNote(null);
    try {
      seed(await api.setTriageConfig(patch));
      setNote({ ok: true });
    } catch (err) {
      setNote({
        ok: false,
        error: err instanceof ApiError ? err.message : "save failed",
      });
    } finally {
      setBusy(false);
    }
  }

  function capField(key: FormKey, label: string) {
    if (!cfg) return null;
    return (
      <div className="set-field" key={key}>
        <label htmlFor={`cap-${key}`}>
          {label}{" "}
          <span className="set-hint">{sourceNote(capSource(cfg, key))}</span>
        </label>
        <input
          id={`cap-${key}`}
          type="number"
          min={1}
          max={100000}
          step={1}
          value={form[key]}
          onChange={(e) => {
            setForm((f) => ({ ...f, [key]: e.target.value }));
            setNote(null);
          }}
          autoComplete="off"
          spellCheck={false}
        />
      </div>
    );
  }

  return (
    <section className="set-section">
      <div className="set-label">Triage budget</div>

      {loadErr ? (
        <div className="set-status err">{loadErr}</div>
      ) : !cfg ? (
        <div className="set-hint">loading…</div>
      ) : (
        <form className="set-form" onSubmit={onSave}>
          {/* STAGE 1 — small LLM, every email --------------------------- */}
          <div className="set-subhead">Stage 1 caps</div>
          <div className="set-hint" style={{ marginTop: -2, marginBottom: 6 }}>
            Stage 1 — {cfg.stage1.model} (every email)
          </div>
          {capField("stage1_global_daily_cap", "global / day")}

          {/* STAGE 2 — capable LLM, escalations only -------------------- */}
          <div className="set-subhead" style={{ marginTop: 14 }}>
            Stage 2 caps
          </div>
          <div className="set-hint" style={{ marginTop: -2, marginBottom: 6 }}>
            Stage 2 — {cfg.stage2_model} (escalations)
          </div>
          {STAGE2_CAP_FIELDS.map(({ key, label }) => capField(key, label))}

          <div className="set-row">
            <button type="submit" className="primary" disabled={busy}>
              {busy ? "saving…" : "Save"}
            </button>
            {note?.ok && (
              <span className="set-status ok">
                <span className="dot" /> saved
              </span>
            )}
            {note && !note.ok && (
              <span className="set-status err">{note.error}</span>
            )}
          </div>

          <TriageEstimator cfg={cfg} />
        </form>
      )}
    </section>
  );
}

/** Per-stage cost math from trailing-14d means; null when a stage has no token
 *  history yet (nothing to price). */
interface StageEstimate {
  costPerCall: number;
  typicalDaily: number;
  ceilingDaily: number;
}

function estimateStage(
  tokensIn: number | null,
  tokensOut: number | null,
  priceIn: number,
  priceOut: number,
  avgCallsPerDay: number,
  cap: number,
): StageEstimate | null {
  if (tokensIn === null || tokensOut === null) return null;
  const costPerCall =
    (tokensIn / 1e6) * priceIn + (tokensOut / 1e6) * priceOut;
  return {
    costPerCall,
    typicalDaily: avgCallsPerDay * costPerCall,
    ceilingDaily: cap * costPerCall,
  };
}

/**
 * Spend estimator, text-only. Each stage's "typical" uses its trailing-14d
 * average call rate × its per-call token means; the combined "hard ceiling" sums
 * each stage's cap × its per-call cost. Stages with no usage history (null token
 * means) are skipped rather than printed as $0.00.
 */
function TriageEstimator({ cfg }: { cfg: TriageConfig }) {
  const inbound = Math.round(cfg.avg_inbound_per_day);

  const s1 = estimateStage(
    cfg.stage1.avg_tokens_in_per_call,
    cfg.stage1.avg_tokens_out_per_call,
    cfg.stage1.price_in_per_mtok,
    cfg.stage1.price_out_per_mtok,
    cfg.stage1.avg_calls_per_day,
    cfg.stage1.global_daily_cap,
  );
  const s2 = estimateStage(
    cfg.avg_tokens_in_per_call,
    cfg.avg_tokens_out_per_call,
    cfg.price_in_per_mtok,
    cfg.price_out_per_mtok,
    cfg.avg_stage2_calls_per_day,
    cfg.global_daily_cap,
  );

  function stageLine(name: string, est: StageEstimate | null) {
    if (!est)
      return <div>{name} typical: not enough usage history yet.</div>;
    return (
      <div>
        {name} typically ~{fmtUsd(est.typicalDaily)}/day.
      </div>
    );
  }

  const priced = [s1, s2].filter((e): e is StageEstimate => e !== null);
  const totalDaily = priced.reduce((a, e) => a + e.typicalDaily, 0);
  const totalCeiling = priced.reduce((a, e) => a + e.ceilingDaily, 0);

  return (
    <div className="set-hint" style={{ marginTop: 6, lineHeight: 1.6 }}>
      <div>You average ~{inbound} emails/day.</div>
      {stageLine("Stage 1", s1)}
      {stageLine("Stage 2", s2)}
      {priced.length > 0 ? (
        <>
          <div>
            typical total ~{fmtUsd(totalDaily)}/day (~{fmtUsd(totalDaily * 30)}
            /mo).
          </div>
          <div>
            combined hard ceiling at your caps: ~{fmtUsd(totalCeiling)}/day.
          </div>
        </>
      ) : (
        <div>Not enough usage history to estimate a total yet.</div>
      )}
    </div>
  );
}

/**
 * RANKING — the blend that orders the Sitrep "For your eyes" zone. A single
 * slider between time (urgency) and severity (importance). The stored pref
 * `rankWeight` is the URGENCY share (0..1); the slider is inverted so dragging
 * RIGHT toward "severity" lowers it — matching the "time ←→ severity" label.
 * The Sitrep re-ranks live via usePref, no save button.
 */
function RankingSection() {
  const rankWeight = usePref("rankWeight");
  // Slider value = severity share (1 - urgency share) so left = time.
  const value = 1 - rankWeight;
  return (
    <section className="set-section">
      <div className="set-label">For your eyes</div>
      <div className="set-field">
        <label htmlFor="set-rank">Rank For your eyes by: time ←→ severity</label>
        <input
          id="set-rank"
          className="set-range"
          type="range"
          min={0}
          max={1}
          step={0.05}
          value={value}
          onChange={(e) => setPref("rankWeight", 1 - Number(e.target.value))}
        />
        <div className="set-range-ends" aria-hidden="true">
          <span>time</span>
          <span>severity</span>
        </div>
      </div>
      <div className="set-hint" style={{ marginTop: 6 }}>
        Blends how soon something is due with how important it is. Overdue items
        rank high; drag toward severity to let important undated mail compete.
      </div>
    </section>
  );
}

// MAIL — reading preferences. Remote images: "Always" renders network images
// automatically in every email; "On demand" blocks them at the frame CSP until
// the reader opts in per message (tracking pixels are stripped either way).
function MailSection() {
  const autoImages = usePref("loadRemoteImages");
  return (
    <section className="set-section">
      <div className="set-label">Mail</div>
      <div className="set-field-inline">
        <span className="set-key">images</span>
        <div className="set-toggle" role="group" aria-label="remote images">
          <button
            type="button"
            className={autoImages ? "active" : ""}
            aria-pressed={autoImages}
            onClick={() => setPref("loadRemoteImages", true)}
          >
            Always
          </button>
          <button
            type="button"
            className={!autoImages ? "active" : ""}
            aria-pressed={!autoImages}
            onClick={() => setPref("loadRemoteImages", false)}
          >
            On demand
          </button>
        </div>
      </div>
      <div className="set-hint" style={{ marginTop: 8 }}>
        Tracking pixels are removed either way; images load with no referrer.
      </div>
    </section>
  );
}

// DEVELOPER — dev-mode toggle. When on, re-triage affordances appear in the
// sitrep masthead (last-7-days) and the thread viewer (this email): they reset
// the LLM verdicts so the stage-1/stage-2/extractor pipeline re-runs — the
// prompt-iteration loop. Rule-decided and sealed rows are never touched.
function DeveloperSection() {
  const dev = usePref("developerMode");
  return (
    <>
      <div className="set-field-inline">
        <span className="set-key">dev mode</span>
        <div className="set-toggle" role="group" aria-label="developer mode">
          <button
            type="button"
            className={!dev ? "active" : ""}
            aria-pressed={!dev}
            onClick={() => setPref("developerMode", false)}
          >
            Off
          </button>
          <button
            type="button"
            className={dev ? "active" : ""}
            aria-pressed={dev}
            onClick={() => setPref("developerMode", true)}
          >
            On
          </button>
        </div>
      </div>
      <div className="set-hint" style={{ marginTop: 8 }}>
        Adds re-triage buttons (sitrep masthead + open email) that re-run the
        triage pipeline. Re-triaging spends model budget.
      </div>
    </>
  );
}
