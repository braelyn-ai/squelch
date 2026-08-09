# Hosted Passband: a fresh Debian box to the first tenant

This is the runbook for the VPS half of the hosted MVP (`docs/HOSTED.md`,
Phase 2). The other half, `squelch-control`, runs on Railway and is not
installed here.

## What you are building

```
                    signup.<base>                warden.<base>
                          |                            |
                    (CNAME to Railway)            (A to this box)
                          |                            |
                   squelch-control  ---- HTTPS ---> squelch-warden   (root, loopback:8852)
                   (Railway)          bearer token        |
                          |                               | writes files, runs systemctl
                          |                               v
                          |                  /etc/squelch/tenants/<label>.env
                          |                  /var/lib/squelch/tenants/<label>/
                          |                  /etc/caddy/tenants/<label>.caddy
                          |                  systemctl enable --now squelchd@<label>
                          |                               |
                          '--- age ciphertext ------------'
                              (encrypted to THIS box's recipient;
                               only the tenant daemons can decrypt)

  <label>.<base>  ---(A to this box)--->  Caddy  --->  127.0.0.1:<port>  squelchd@<label>
```

The trust split is the point, so it is worth stating before you start:

- The **control plane** holds the Google web-client secret and this box's age
  **recipient** (a public key). It encrypts each tenant's credential the moment
  the OAuth exchange returns. It never holds the private key.
- The **warden** holds root on this box. It writes the ciphertext it is given
  and never reads it: it has no age identity, by design.
- The **tenant daemons** hold the age **identity** and are the only processes
  that decrypt anything.

## 0. What you need

- A Debian 13 (trixie) box. Bookworm's glibc is too old for the embedding
  runtime the daemon links (`ort` needs 2.38+), so trixie is not optional.
- A domain. This runbook writes `passband.email`; substitute yours everywhere.
- The Railway service for `squelch-control` already created (you need its
  hostname for one DNS record, and it needs two values from step 3).

## 1. DNS

| Name | Type | Value | Why |
|---|---|---|---|
| `passband.email` | A | your box IP (or the marketing site) | optional |
| `*.passband.email` | A | your box IP | every tenant's subdomain |
| `warden.passband.email` | A | your box IP | the control plane's way in |
| `signup.passband.email` | CNAME | your Railway app hostname | signup never touches this box |

The wildcard is what makes a new tenant instant: provisioning creates a Caddy
site for `alice.passband.email` and DNS already answers for it.

## 2. Box preparation

```sh
apt update && apt install -y curl ca-certificates debian-keyring debian-archive-keyring apt-transport-https age

# The one unprivileged account every tenant daemon runs as.
useradd --system --home-dir /var/lib/squelch --create-home --shell /usr/sbin/nologin squelch

install -d -o root -g root -m 0755 /etc/squelch
install -d -o root -g root -m 0755 /etc/squelch/tenants
install -d -o squelch -g squelch -m 0755 /var/lib/squelch
install -d -o squelch -g squelch -m 0755 /var/lib/squelch/tenants
install -d -o root -g root -m 0700 /var/lib/squelch/warden
```

`/var/lib/squelch/tenants` is 0755 and owned by `squelch`; each tenant's
directory inside it is 0700, created by the warden.

## 3. The age identity

This is the key to every mailbox on the box. Generate it once:

```sh
install -d -o root -g squelch -m 0750 /etc/squelch/age
age-keygen -o /etc/squelch/age/identity.txt
chown root:squelch /etc/squelch/age/identity.txt
chmod 0640 /etc/squelch/age/identity.txt
```

`age-keygen` prints the **recipient** (`age1...`) on stderr and writes the
identity to the file. Two things happen with those:

- The recipient goes into the control plane's environment as
  `SQUELCH_CONTROL_AGE_RECIPIENT`. It is a public key; it may be committed to a
  deploy config, pasted in chat, whatever.
- The identity stays in that file, readable by root and by the `squelch` group,
  and is named in every tenant's env file as `SQUELCH_CRED_AGE_IDENTITY`. It
  never leaves this box. Back it up somewhere you would put a root password:
  **without it, every stored credential on the box is unrecoverable** and every
  tenant has to sign in with Google again.

One identity for the whole box is a documented MVP choice, not a claim of
per-tenant cryptographic isolation. See "What this does not isolate" below.

## 4. Binaries

Build on a matching box (or cross-build and copy):

```sh
git clone https://github.com/braelyn-ai/squelch && cd squelch
apt install -y build-essential g++ pkg-config
cargo build --release --locked -p squelchd -p squelch-warden
install -m 0755 target/release/squelchd /usr/local/bin/squelchd
install -m 0755 target/release/squelch-warden /usr/local/bin/squelch-warden
```

The daemon downloads its embedding weights on first run, into
`$HOME/.local/share/squelch/models` — and `HOME` is per tenant, so each tenant
pays for its own copy (about 130 MB). To share one copy instead, pre-seed it and
symlink after the first tenant is up:

```sh
# after tenant one has downloaded them
install -d -o squelch -g squelch /var/lib/squelch/models
mv /var/lib/squelch/tenants/<first>/.local/share/squelch/models/* /var/lib/squelch/models/
# then, per tenant:
sudo -u squelch ln -sfn /var/lib/squelch/models \
  /var/lib/squelch/tenants/<label>/.local/share/squelch/models
```

Do that with tenants stopped: two daemons downloading into one directory at the
same time is a race nobody has made safe.

## 5. systemd

```sh
install -m 0644 deploy/hosted/squelchd@.service /etc/systemd/system/squelchd@.service
install -m 0644 deploy/hosted/warden.service /etc/systemd/system/warden.service

# The warden's own environment. 0600: it holds the bearer token.
umask 077
cat > /etc/squelch/warden.env <<EOF
SQUELCH_WARDEN_TOKEN=$(openssl rand -base64 32)
SQUELCH_WARDEN_BASE_DOMAIN=passband.email
SQUELCH_WARDEN_AGE_IDENTITY=/etc/squelch/age/identity.txt
EOF
chmod 0600 /etc/squelch/warden.env

systemctl daemon-reload
systemctl enable --now warden
curl -fsS http://127.0.0.1:8852/healthz && echo
```

Copy that token into the control plane's environment as
`SQUELCH_CONTROL_WARDEN_TOKEN` (with `SQUELCH_CONTROL_WARDEN_URL=https://warden.passband.email`).
It is the only credential between the two halves, and it is root on this box:
rotate it by editing this file and restarting both services.

Everything else the warden reads has a default that matches this runbook; the
full table is in `squelch-warden/README.md`.

## 6. Caddy

```sh
curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' \
  | gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' \
  | tee /etc/apt/sources.list.d/caddy-stable.list
apt update && apt install -y caddy

install -d -o root -g root -m 0755 /etc/caddy/tenants
install -m 0644 deploy/hosted/Caddyfile /etc/caddy/Caddyfile
# edit /etc/caddy/Caddyfile: your base domain and your ACME email
systemctl reload caddy
```

The warden runs `systemctl reload caddy` after it writes a tenant site file, so
the `caddy` unit must be exactly that name (or set
`SQUELCH_WARDEN_CADDY_UNIT`).

Confirm the warden is reachable and refuses strangers:

```sh
curl -sS -o /dev/null -w '%{http_code}\n' https://warden.passband.email/v1/tenants   # 401
curl -sS https://warden.passband.email/healthz                                        # ok
```

## 7. The first tenant

Normally the control plane does this at the end of a signup. To prove the box
works before wiring signup up, do it by hand with a credential you can throw
away:

```sh
RECIPIENT=$(age-keygen -y /etc/squelch/age/identity.txt)
CT=$(printf '{"refresh_token":"not-a-real-token"}' | age -a -r "$RECIPIENT")
TOKEN=$(grep SQUELCH_WARDEN_TOKEN /etc/squelch/warden.env | cut -d= -f2-)

curl -sS -X POST https://warden.passband.email/v1/tenants \
  -H "authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' \
  -d "$(jq -n --arg ct "$CT" '{label:"test1",account_email:"you@example.com",cred_read_ciphertext:$ct}')"
```

A 201 comes back with the port, the pairing code, the tenant URL, and the
`passband://pair?...` deep link. Then:

```sh
systemctl status squelchd@test1
curl -sS -o /dev/null -w '%{http_code}\n' https://test1.passband.email/healthz
curl -sS -X DELETE -H "authorization: Bearer $TOKEN" \
  https://warden.passband.email/v1/tenants/test1        # 204
```

The daemon will fail to sync with that fake credential, which is expected: what
this proves is the unit, the route, the certificate, and the pairing exec.

`DELETE` stops and disables the unit and removes the Caddy site. It KEEPS the
data directory and the env file, so nothing here destroys a mailbox. Cleaning up
a test tenant fully is manual:

```sh
rm -rf /var/lib/squelch/tenants/test1 /etc/squelch/tenants/test1.env
# and drop its entry from /var/lib/squelch/warden/state.json
```

## Operating notes

**The state file.** `/var/lib/squelch/warden/state.json`, mode 0600, one entry
per tenant with its port and status. It is written temp-then-rename, so it is
never half a document. If it is ever lost, the warden does NOT rebuild it
automatically: an empty state would hand port 9100 to the next signup while a
live tenant is already there. Rebuild it by hand from the env files, which carry
the port and the account:

```sh
for f in /etc/squelch/tenants/*.env; do
  label=$(basename "$f" .env)
  port=$(grep '^SQUELCH_BIND=' "$f" | cut -d: -f2)
  email=$(grep '^SQUELCH_ACCOUNT_EMAIL=' "$f" | cut -d= -f2-)
  echo "$label $port $email"
done
```

...then write those into the JSON shape the warden reads (`version`, `tenants`
keyed by label, each with `label`, `account_email`, `port`, `status`,
`created_at`) and restart it.

**A tenant is stuck in `provisioning`.** That means a warden died mid-provision.
`GET /v1/tenants/<label>` reports it as `failed`. `DELETE` it, then let the
control plane re-run the signup.

**Pairing a second device.** `POST /v1/tenants/<label>/pair` re-mints a code.
This supersedes the previous one, which is the daemon's documented behaviour:
one live pairing code per account.

**Rotating the warden token.** Edit `/etc/squelch/warden.env`, restart `warden`,
update the control plane. There is no grace period; a signup in flight during
the swap fails and the user retries.

**Backups.** Two things matter and they are different sizes:
`/etc/squelch/age/identity.txt` (a few hundred bytes, and everything depends on
it) and `/var/lib/squelch/tenants/*/squelch.db` (the mail index). Litestream for
the second one is Phase 2 work that is not in this runbook yet.

## What this does not isolate

Worth being honest about, because the hosted pitch is per-tenant process
isolation and this is where that claim stops:

- **Every tenant daemon runs as the same `squelch` user.** Tenant directories
  are 0700, which keeps them away from every other account on the box but not
  from each other: a compromised daemon could read another tenant's data dir.
  Isolation here is process-level (separate units, separate SQLite files,
  separate memory), not user-level. Per-tenant system users are the hardening
  step, and they change what the warden's chown does.
- **One age identity for the whole box.** Encryption at rest protects against a
  stolen disk or a leaked backup, not against root on a running box. Per-tenant
  identities need a key-per-tenant story and are deliberately not in the MVP.
- **The warden runs as root.** Its bearer token is the box. It binds loopback
  and Caddy fronts it, so the token never crosses the network unencrypted, but
  there is no second factor between the control plane and root here.

## The agent door is not served

Hosted ships the human door only. `/mcp` and `/mcp/*` are refused at Caddy in
two places: inline in every generated tenant site, and as the `no_agent_door`
snippet in the main Caddyfile. The daemon still serves the agent door on
loopback, so anything on the box can reach it; nothing off the box can.
