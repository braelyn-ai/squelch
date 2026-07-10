#!/usr/bin/env bash
# Back up / restore squelch sender rules across db resets.
#
# The schema applies fresh on every open (no migrations yet), so any schema
# change wipes the sender_rules table. Rules are user intent, not derived data —
# they should survive a reset. This dumps them to a git-ignored JSON and
# re-applies them through the human-door API.
#
#   scripts/rules.sh backup    # save current rules to scripts/rules.backup.json
#   scripts/rules.sh restore   # re-create saved rules via POST /client/rules
#
# Reads SQUELCH_API_TOKEN and SQUELCH_BIND (default 127.0.0.1:8848) from .env.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
set -a; source "$here/.env"; set +a
base="http://${SQUELCH_BIND:-127.0.0.1:8848}"
auth="Authorization: Bearer ${SQUELCH_API_TOKEN}"
store="$here/scripts/rules.backup.json"

case "${1:-}" in
  backup)
    curl -fsS -H "$auth" "$base/client/rules" > "$store"
    echo "saved $(python3 -c "import json,sys;print(len(json.load(open('$store'))))") rules to $store"
    ;;
  restore)
    [ -f "$store" ] || { echo "no backup at $store" >&2; exit 1; }
    python3 - "$store" "$base" "$SQUELCH_API_TOKEN" <<'PY'
import json, sys, urllib.request
store, base, token = sys.argv[1], sys.argv[2], sys.argv[3]
rules = json.load(open(store))
existing = json.load(urllib.request.urlopen(
    urllib.request.Request(f"{base}/client/rules", headers={"Authorization": f"Bearer {token}"})))
have = {(r["match_pattern"], r["disposition"]) for r in existing}
made = 0
for r in rules:
    key = (r["match_pattern"], r["disposition"])
    if key in have:
        continue
    body = json.dumps({"match_pattern": r["match_pattern"],
                       "want": r.get("want_text", ""),
                       "disposition": r["disposition"]}).encode()
    req = urllib.request.Request(f"{base}/client/rules", data=body, method="POST",
        headers={"Authorization": f"Bearer {token}", "Content-Type": "application/json"})
    urllib.request.urlopen(req)
    made += 1
print(f"restored {made} rules ({len(rules)-made} already present)")
PY
    ;;
  *)
    echo "usage: scripts/rules.sh {backup|restore}" >&2; exit 2 ;;
esac
