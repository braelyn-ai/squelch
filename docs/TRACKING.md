# Read tracking

Squelch can attach a 1×1 tracking pixel to mail you send and report when it was
fetched. It is off by default, opt-in per send, and the canonical record never
leaves your daemon.

This document is the deployment model. For the privacy invariants around
*incoming* trackers, see [SECURITY.md §3](SECURITY.md).

## What an "open" is worth

Less than it sounds, and the UI says so. Two things routinely fetch the pixel
without a human reading anything:

- **Apple Mail Privacy Protection** pre-fetches images through Apple's proxy,
  often minutes after delivery, for every message.
- **Gmail** proxies images through `googleusercontent`, which hides the
  recipient's IP and user agent but still fetches.

Every open is stored with its user agent and a `classification`: `proxied` when
the fetch came from a known image proxy, `unknown` otherwise. Passband renders
these as "opened" and "opened (via proxy)". Treat the signal as *probably
opened*, never as certified mail. There is no way to distinguish a proxy
pre-fetch from a real read, and any product that claims otherwise is guessing.

## The two switches

Read tracking has two independent settings, and confusing them is the most
common way to end up with a feature that silently records nothing.

| Setting | Gates | Effect when unset |
|---|---|---|
| `[tracking] base_url` (`SQUELCH_TRACK_URL`) | **minting** — where the pixel points | No send is ever tracked, whatever the client asks for |
| `[pusher] relay_url` (`SQUELCH_RELAY_URL`) | **collection via relay** — whether the daemon drains a relay | No opens poller; opens must arrive at the daemon directly |

`base_url` decides which door the recipient's mail client knocks on. That is the
only thing it decides. It does not have to be a relay, and for self-hosted
deployments it usually should not be.

The daemon states its posture at startup, so you never have to infer it:

```
squelchd: read tracking enabled, pixel at https://track.example.com/t/{token}
squelchd:   no relay configured, so opens are recorded ONLY if that URL reaches
            THIS daemon's /t/ route. If it points at a relay, opens will buffer
            there and never be collected.
```

## Mode A — direct expose (recommended for self-hosting)

The daemon serves the pixel itself. No relay, no shared secret, no third party
in the path.

```
recipient's mail client ──HTTPS──▶ tunnel ──▶ squelchd /t/{token} ──▶ your SQLite
```

Point a tunnel at the daemon's HTTP port and set `base_url` to its public
address:

```toml
[tracking]
base_url = "https://track.yourdomain.example"
```

Any tunnel works — Cloudflare Tunnel, Tailscale Funnel, a reverse proxy on a box
you already run. `GET /t/{token}` is served unauthenticated by necessity: a
stranger's mail client has no bearer token and never will. It is the only
unauthenticated route on the daemon, and it is built to leak nothing — see
"What the pixel route does and does not do" below.

**The trade-off, stated plainly:** mail clients do not retry a failed image
fetch. If the tunnel is down, or the machine is asleep, when the recipient opens
your mail, that open is lost permanently and silently. A laptop that sleeps is a
poor pixel host. This is the main reason to want a relay.

**Deliverability note:** the hostname in the pixel URL appears in every tracked
message you send. A stable, boring domain you control is better than a rotating
tunnel hostname, which some filters read as a mild spam signal.

## Mode B — relay

A small always-on service holds the pixel and buffers opens until your daemon
collects them. The daemon still owns the canonical record; the relay is a
**mailbox, not a database**.

```
recipient ──▶ relay /t/{token} ──▶ buffer
                                     │
squelchd ──poll /v1/opens?cursor=N───┘──▶ your SQLite   (ack deletes what it covers)
```

The relay only ever sees an opaque token, a timestamp, and a user agent. It
cannot map a token to a recipient, a subject, an account, or a message — that
mapping exists only in your daemon's `send_trackers` table. Rows live for
seconds to minutes: the daemon drains every 60 seconds and the cursor deletes
what it has durably stored. The steady state is an empty table.

Run your own with the repo-root `Dockerfile`:

```
SQUELCH_RELAY_AUTH_TOKEN=<64+ random chars, one unbroken line>
SQUELCH_RELAY_DB_PATH=/data/opens.sqlite3     # else the buffer is in memory
SQUELCH_RELAY_APNS_*                          # only needed for iOS push
```

Then, on the daemon, point **both** switches at it and use the same bearer:

```toml
[tracking]
base_url = "https://relay.yourdomain.example"

[pusher]
relay_url = "https://relay.yourdomain.example"
relay_token = "<the same token the relay checks>"
```

Mounting a volume for `SQUELCH_RELAY_DB_PATH` is what makes the buffer survive a
restart. Without it the relay still runs, but any open the daemon has not yet
drained dies on every redeploy. The container chowns the mount point before
dropping privileges, so the volume may arrive root-owned.

### The relay bearer is not a user account

`SQUELCH_RELAY_AUTH_TOKEN` is an **abuse gate**, not a tenancy boundary. It
exists so strangers cannot spend the operator's APNs quota. It assumes one owner
on both ends.

**Do not share one relay bearer between two daemons.** The open buffer has no
tenant column and `/v1/opens` has a single global cursor, so two daemons draining
the same relay will ack-delete each other's rows — each silently never sees the
opens the other collected. Multi-tenant relay operation is unbuilt; until it
exists, one relay serves one daemon.

An anonymous relay (`SQUELCH_RELAY_ALLOW_ANONYMOUS=1`) does not serve `/v1/opens`
at all. Serving the push route open is a supported nuisance; serving the drain
open would hand strangers live tracking tokens and let them wipe the buffer on
the way out.

## Which mode for which tier

- **Self-hosted:** Mode A unless you need always-on collection. You do not need
  anyone else's relay for read tracking — the pixel is ordinary HTTP and your
  daemon already serves it. (APNs push is different: it requires an Apple
  developer key you may not have, which is the one thing a self-hoster genuinely
  cannot do alone.)
- **Hosted:** the operator runs both the daemon and the relay, provisions the
  bearer on both ends, and the user configures nothing.

## What the pixel route does and does not do

Both the daemon's and the relay's `GET /t/{token}` behave identically:

- **Always 200**, always the same 1×1 transparent GIF, with `no-store`. A known
  token, an unknown token, a malformed one, and a failed write are
  byte-indistinguishable. There is no 404 to probe with.
- **Reads nothing.** It appends one row and returns an image. No mail, no
  metadata, no ids reach the response.
- **Records nothing for a token it does not recognise**, so unsolicited traffic
  cannot grow the table. Tokens outside the minted shape (16–64 url-safe base64
  characters) are rejected before the store is touched.
- **Bounded.** Opens are capped per token on both sides, because a live pixel URL
  is a capability anyone holding it can refetch forever. The daemon additionally
  caps how many pixel writes may be in flight, since this is the only
  unauthenticated route that touches the store the whole daemon shares.

Tokens are 192 bits of OS entropy and are never logged, audited, or sent to the
client. **They do, however, ride as a URL path segment**, so whatever fronts your
public base URL — a CDN, nginx, Railway's edge — will record live tokens in its
access logs. The daemon and the relay keep that promise; your reverse proxy was
never asked to.

## Incoming trackers

Passband strips tracking pixels out of mail you *receive*, with one deliberate
exception: senders you have written to before are allowed to see that you opened
their mail. The carve-out is scoped to the tracking-pixel strip only — the image
proxy, the CSP, and the ingest sanitizer all still apply, and remote images
remain off by default. See [SECURITY.md §3](SECURITY.md).
