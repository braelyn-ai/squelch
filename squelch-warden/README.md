# squelch-warden

The VPS-side tenant provisioner for hosted Passband. One box, one warden, one
squelchd per tenant under a systemd template unit, each on its own loopback port
behind Caddy at `<label>.<base domain>`.

The control plane (`squelch-control`, on Railway) is the only caller. It sends
four JSON requests; the warden does every local thing a signup implies.

Runbook for a fresh box: [`deploy/hosted/SETUP.md`](../deploy/hosted/SETUP.md).
Architecture and the decisions behind it: [`docs/HOSTED.md`](../docs/HOSTED.md).

## What it is trusted with

Root on one box. It creates systemd units and writes files as root, so its
bearer token is the box.

**Not** anyone's mail and **not** anyone's tokens. A credential arrives as age
ciphertext that the control plane encrypted to this box's recipient, and the
warden writes it to disk verbatim: it holds no age identity, so it cannot read
what it stores. The identity belongs to the tenant daemons, which are the only
processes that decrypt.

That is enforced, not just intended: a body that is not ASCII-armored age is
refused with a 422 before anything touches the filesystem. If the control plane
ever handed over a plaintext refresh token, this is the last place on the path
that can notice, and it does.

The crate does not depend on `squelch-core`. No store, no mail parser, no OAuth
client.

## Wire

Every `/v1` route takes `Authorization: Bearer $SQUELCH_WARDEN_TOKEN`, compared
in constant time. Anything else is a bare 401 with no body.

| Route | Success | Failures |
|---|---|---|
| `POST /v1/tenants` | `201 { port, pair_code, pair_url, deep_link }` | `409` label exists, `422` invalid label / address / ciphertext, `400` malformed JSON |
| `GET /v1/tenants/{label}` | `200 { status, port }` where status is `active` / `failed` / `stopped` | `404` |
| `DELETE /v1/tenants/{label}` | `204` (stop + disable, route removed, **data dir kept**) | - |
| `POST /v1/tenants/{label}/pair` | `200 { pair_code, pair_url, deep_link }` | `404` |
| `GET /healthz` | `200 ok` (no token: it is a liveness probe and says nothing) | - |

`POST /v1/tenants` body:

```json
{
  "label": "alice",
  "account_email": "alice@example.com",
  "cred_read_ciphertext": "-----BEGIN AGE ENCRYPTED FILE-----\n...\n-----END AGE ENCRYPTED FILE-----\n"
}
```

`DELETE` is idempotent: an unknown label is `204`, because the control plane
calls it on its own unwind paths and should not have to special-case a 404
there.

A `500` body is a machine reason and nothing else (`unit_start_failed`,
`caddy_reload_failed`, `pair_failed`, `ports_exhausted`, ...). No path, no OS
error, no mailbox address ever crosses the wire. The detail is in this box's
journal.

## What a provision does, in order

1. allocate the lowest free port from 9100 (skipping ports the state file
   claims and ports something is listening on)
2. `mkdir /var/lib/squelch/tenants/<label>` at 0700
3. write the ciphertext to `credentials.json` at 0600, verbatim
4. `chown -R squelch:squelch` the tenant directory
5. render `/etc/squelch/tenants/<label>.env`
6. render `/etc/caddy/tenants/<label>.caddy` and `systemctl reload caddy`
7. `systemctl enable --now squelchd@<label>`
8. `setpriv --reuid=squelch ... squelchd pair --url https://<label>.<base>` and
   parse the code and deep link out of its output

Any failure unwinds what it created, in reverse, best effort: the unit is
stopped and disabled, the site file removed and Caddy reloaded, the env file and
the freshly created data directory removed, and the state record dropped. No
half-enabled unit, no route to a port nothing listens on, no leaked port.

The pair step drops to the tenant user on purpose. Run as root it would leave
root-owned `-wal` and `-shm` files beside the tenant's SQLite database, and the
daemon would fail its next write with an error nobody would trace back here.

## Environment

| Variable | Default | Notes |
|---|---|---|
| `SQUELCH_WARDEN_TOKEN` | **required** | 32+ characters. Refuses to start below that; never logged. |
| `SQUELCH_WARDEN_BASE_DOMAIN` | **required** | `passband.email`. Tenants are subdomains of it. |
| `SQUELCH_WARDEN_AGE_IDENTITY` | **required** | Path only. Named in each tenant's env file; the warden never reads it. |
| `SQUELCH_WARDEN_BIND` | `127.0.0.1:8852` | Keep it loopback; Caddy fronts it with TLS. |
| `SQUELCH_WARDEN_STATE_DIR` | `/var/lib/squelch/warden` | Holds `state.json`, mode 0600. |
| `SQUELCH_WARDEN_TENANTS_DIR` | `/var/lib/squelch/tenants` | |
| `SQUELCH_WARDEN_ENV_DIR` | `/etc/squelch/tenants` | |
| `SQUELCH_WARDEN_CADDY_DIR` | `/etc/caddy/tenants` | Imported by the main Caddyfile with a glob. |
| `SQUELCH_WARDEN_SQUELCHD_BIN` | `/usr/local/bin/squelchd` | |
| `SQUELCH_WARDEN_SETPRIV_BIN` | `/usr/bin/setpriv` | |
| `SQUELCH_WARDEN_SYSTEMCTL_BIN` | `/usr/bin/systemctl` | |
| `SQUELCH_WARDEN_CHOWN_BIN` | `/usr/bin/chown` | |
| `SQUELCH_WARDEN_TENANT_USER` | `squelch` | Owner of every tenant directory. |
| `SQUELCH_WARDEN_CADDY_UNIT` | `caddy` | Reloaded after a site file changes. |
| `SQUELCH_WARDEN_PORT_BASE` | `9100` | 900 ports from here; well below the ephemeral range. |
| `SQUELCH_WARDEN_LOG` | `info` | `tracing` filter. |

## State

One flat JSON file, `state.json`, mode 0600, written temp-then-rename. Tens of
tenants, not thousands: SQLite would buy nothing but a migration story. It holds
labels, ports, statuses, stamps, and the account address (which is what
re-rendering an env file needs, and which never reaches a log line or a
response).

A state file that will not parse is a **refusal**, not an empty state. An empty
state would hand a live tenant's port to the next signup. Rebuilding it from the
env-file directory is a documented manual recovery in SETUP.md.

## Logging

Counts, statuses, and the tenant label. Never a mailbox address, never a
credential, and never a command's output: `squelchd pair` prints a live pairing
code on stdout, so the rule is blanket rather than per-call.

## Tests

```sh
cargo test -p squelch-warden
```

Every side effect goes through the `Fs` and `CommandRunner` traits, so the
suite needs no systemd, no Caddy, no squelchd, and no root. It asserts the exact
command sequence and the exact file contents (including modes) of a provision,
the unwind at three different failure points, port allocation across a restart,
and a 401 for every way of getting the bearer wrong.
