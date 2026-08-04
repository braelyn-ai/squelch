# Hosted squelch: the plan

Status: planned 2026-08-03, not yet started. This is the decision record and roadmap
for offering squelch beyond "clone the repo and run cargo".

## Naming (decided 2026-08-03)

The user-facing product is **Passband**; the daemon stays **squelchd** (the
dockerd/git-plumbing pattern). The name is the other half of the same radio
metaphor: squelch mutes everything below the threshold, and the passband is the
band the filter lets through — the daemon kills the noise, Passband is where you
see what made it through.

- Mac client: **Passband.app**. Repo, crates, and binary names stay `squelch*`.
- Domains (all unregistered as of 2026-08-03 — **register immediately**):
  `passband.app` (product/homepage), `passband.email` (hosted:
  `<user>.passband.email`, consent relay at `auth.passband.email`),
  `passband.io` (defensive).
- Deep link scheme: `passband://`.
- Collision search found no software product named Passband; a proper USPTO
  trademark search is still owed before Phase 0 paperwork is filed under the name.
- Rename pass (when shipping starts): Swift client bundle/display name — note the
  bundle-identifier change will re-prompt keychain ACLs — plus README reframing
  ("Passband, powered by squelchd") and the Google consent screen name.

## The two tiers

There is a clean philosophical line and exactly two products on either side of it:

| Tier | Daemon runs | Gmail token lives | Who it's for |
|---|---|---|---|
| **Self-host** | your machine (docker image) | your disk/keyring; our infra cryptographically cannot mint it | privacy/control people, NAS and homelab crowd |
| **Hosted** | our infra, one daemon per user | our token store, KMS-encrypted | "sign in with Gmail and it just works", always-on triage, iOS |

Decisions made and closed:

- **No middle tier.** A "Local+" (daemon on your Mac, hosted onboarding) was considered
  and rejected: it can't do iOS or internet-reachable MCP, which are the reasons to
  want hosted at all. People who want local custody self-host.
- **No web UI for hosted.** The web surface is signup only. The product is the native
  clients plus the MCP URL.
- **No end-to-end-encryption story for hosted.** Hosted means we hold it: encrypted at
  rest, per-tenant process isolation, honest about it. The half-crypto alternatives
  (bodies local / index hosted, etc.) complicate everything and convince nobody. The
  privacy product is self-host.

## OAuth architecture

The load-bearing subtlety: a refresh token is bound to the OAuth client that minted it.
If self-hosted daemons got tokens via a confidential broker client, they would need the
broker for **every hourly access-token refresh** — our uptime becomes their dependency
and tokens transit our infra forever. So the two tiers use two different OAuth clients
under **one GCP project and one consent screen** ("Squelch"):

- **Self-host: Desktop-type client, credentials embedded in the docker image.**
  Google treats installed-app client secrets as non-confidential by design; this is
  the sanctioned model rclone and Thunderbird use. After consent, the daemon exchanges
  and refreshes tokens directly with Google. Our infra is never in the loop again.
- **Hosted: Web-type confidential client.** The control plane runs a normal
  server-side flow and stores the refresh token encrypted, because holding it is the
  product.

### The consent relay (broker for self-host)

Headless docker (NAS, VPS) has no browser on the host, and the current answer
(`ssh -L` port forwarding) is miserable. The broker fixes consent UX without ever
being trusted:

1. Daemon generates a PKCE verifier + session id, prints
   `https://auth.passband.email/link?s=<session>` to its logs.
2. User opens that URL on any device, lands on Google consent.
3. Google redirects the auth code to the broker; the broker parks it in memory
   (short TTL, one-time claim).
4. The daemon polls, claims the code, and exchanges it **itself** with the embedded
   client credentials + PKCE verifier.

The broker only ever sees an auth code that is cryptographically useless without the
verifier, which never leaves the daemon. We are not trusted with tokens because we're
nice; we are incapable of minting them. This sentence belongs on the website. The
relay deploys next to the existing APNs relay (Railway).

> **Correction, 2026-08-04: steps 3 and 4 above cannot be built.** Google permits
> Desktop-type clients to redirect only to loopback, so `auth.passband.email/callback`
> can never be registered on the self-host client and the code never reaches the
> broker. The crate exists (`squelch-broker`, built and audited) but this flow is
> not deployable for self-host. The replacement, which keeps every property this
> section wanted, is in `docs/BROKER.md`: consent runs on a machine that has a
> browser (sanctioned loopback flow, unchanged), and the broker becomes an
> end-to-end-encrypted courier for the resulting token rather than a parker of auth
> codes. Hosted's web client is unaffected: an https callback is correct there, and
> the crate already implements it.

Status: implemented in-repo as `squelch-broker` (2026-08-04) to the wire contract in
`docs/BROKER.md`, deployment pending.

### One project, one verification

`gmail.readonly` / `gmail.modify` are restricted scopes: any public offering needs
OAuth verification plus an annual CASA Tier 2 assessment, with a 100-user cap until it
clears. Both clients under one project means one verification and one CASA. Tradeoff
accepted: shared quota and shared blast radius if the embedded desktop creds are
abused by an impersonator. If that becomes real, split projects and eat the second
CASA then.

## Self-host as a first-class product

Target UX:

```sh
docker run -v squelch-data:/data -p 8848:8848 ghcr.io/<org>/squelch
```

- Multi-arch image (linux/amd64 + linux/arm64 — the NAS/Pi crowd is exactly this
  audience), built and pushed from CI to GHCR.
- First run with no credentials → prints the consent-relay URL. Config is already
  env-var driven, SQLite lives in the volume, and the file credential backend
  (`squelch-core/src/credentials.rs`) already exists for headless hosts. This tier is
  mostly packaging plus the relay, not new architecture.
- BYOK LLM via `ANTHROPIC_API_KEY` as today; ship a compose example and the
  tailnet/MCP story in the image README.

## Hosted architecture

**Per-user daemon, not a multi-tenant rewrite.** squelchd stays single-tenant; that is
a feature: hard tenant isolation, one user's mail can never leak into another's
process, and the sealed-mail / two-door guarantees carry over untouched. One SQLite
per user, Litestream to object storage from day one.

**New crate: `squelch-control`** — the only genuinely new component (axum, like
everything else):

- Signup page → Google OAuth (web client) → refresh token encrypted into the tenant's
  file-backend credential store.
- Provisioning behind a trait (`Provisioner`); first impl is systemd template units
  (`squelchd@<user>`) on a single VPS. The trait is what lets the backend graduate
  later without a rearchitecture.
- Routing: `<user>.passband.email` subdomains via Caddy + wildcard cert. Subdomains over
  path-prefixes for clean per-tenant cookie/CORS isolation forever.
- Invite-code gate for launch. **No Stripe in the MVP** — billing is Phase 3.
- Signup → native app handoff: finish signup, page shows a `passband://pair?token=…`
  deep link / QR.

**Changes to existing code (all in service of hosted, all benefiting self-host too):**

- **Human door auth graduates from one static env-var bearer to issued per-device
  tokens with revocation.** This is the largest change to existing code in the plan
  and a prerequisite for the pairing flow.
- `/mcp` gets real bearer auth once internet-facing (today: localhost trust +
  allowed-hosts). Later, MCP-spec OAuth so claude.ai can connect natively — marquee
  feature: point Claude at your mailbox intelligence with no localhost anywhere.
- Credential file backend gains encryption-at-rest with a per-tenant key (KMS/age).
- LLM triage in hosted runs on our key; the existing per-user budget caps and cost
  ledger become the pricing mechanism. BYOK stays as an option.

## Scaling path ("properly")

Considered and rejected: an Elixir rewrite. The value of this codebase is the Rust
triage/seal/sync engine, and the thing BEAM buys — cheap concurrent per-tenant
processes — tokio already provides. One BEAM node holding every tenant's tokens and
mail in a single address space is a *weaker* isolation story than process-per-tenant.
Also rejected: k8s-with-10,000-pods (one pod per user does not age well).

The actual path, in order:

1. **Now — one VPS.** An idle mailbox daemon is tens of MB; a single 64GB box
   plausibly carries 1–2k users. This gets embarrassingly far, and the 100-user
   verification cap makes it moot anyway.
2. **Next — Gmail push.** Hosted daemons switch from 45s polling to `users.watch` +
   Pub/Sub; the control plane receives pushes and pokes the right daemon. Idle
   tenants become nearly free and API quota stays flat as users grow. This is the
   single highest-leverage scaling change, not orchestration. (Self-host keeps
   polling; containers can't receive Pub/Sub.)
3. **At real scale — fleet mode inside squelchd.** One process hosting N tenants,
   each with its own SQLite file and its own tokio task tree, same sealed/two-door
   code paths. The Elixir benefit without the rewrite. Then boring k8s or Fly
   Machines orchestrates a few dozen fleet nodes instead of thousands of per-user
   pods. Deferred: the `Provisioner` trait means this choice does not have to be
   made today.

## Phasing

1. **Phase 0 — paper (start immediately, gates everything public):** homepage,
   privacy policy, data-handling doc, Google verification + CASA for the one project
   with both clients. Longest lead time, zero code.
2. **Phase 1 — self-host as a product:** GHCR multi-arch image, consent relay in the
   broker, first-run auth UX. Ships value to real users while verification grinds,
   and de-risks the consent patterns hosted signup reuses. Status: the broker is
   implemented in-repo as `squelch-broker` (2026-08-04) per `docs/BROKER.md`,
   deployment pending.
3. **Phase 2 — hosted MVP:** `squelch-control`, systemd provisioning on the VPS,
   human-door issued tokens + `/mcp` bearer auth, web signup → app pairing,
   Litestream, invite codes.
4. **Phase 3 — grow up:** Gmail push, Stripe, fleet-mode vs Fly Machines decision,
   iOS against hosted.
