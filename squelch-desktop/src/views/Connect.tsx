// First-run Connect screen: server URL + API token, tested via /client/stats,
// saved through the tauri set_settings command (keyring). No token ever logged.

import { useState } from "react";
import { useStore } from "../state";

export function Connect() {
  const connect = useStore((s) => s.connect);
  const connStatus = useStore((s) => s.connStatus);
  const connError = useStore((s) => s.connError);

  const [url, setUrl] = useState("http://127.0.0.1:8848");
  const [token, setToken] = useState("");

  const busy = connStatus === "connecting";

  async function onSubmit(e: React.FormEvent) {
    e.preventDefault();
    if (!url.trim() || !token.trim()) return;
    await connect(url.trim(), token.trim());
  }

  return (
    <div className="connect">
      <form className="connect-card" onSubmit={onSubmit}>
        <h1>squelch</h1>
        <p className="sub">connect to your human door</p>

        <div className="field">
          <label htmlFor="url">server url</label>
          <input
            id="url"
            value={url}
            onChange={(e) => setUrl(e.target.value)}
            placeholder="http://127.0.0.1:8848"
            autoComplete="off"
            spellCheck={false}
          />
        </div>

        <div className="field">
          <label htmlFor="token">api token</label>
          <input
            id="token"
            type="password"
            value={token}
            onChange={(e) => setToken(e.target.value)}
            placeholder="SQUELCH_API_TOKEN"
            autoComplete="off"
            spellCheck={false}
          />
        </div>

        {connError && <div className="err">{connError}</div>}

        <button type="submit" disabled={busy} style={{ width: "100%" }}>
          {busy ? "testing…" : "connect"}
        </button>
      </form>
    </div>
  );
}
