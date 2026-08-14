# Hosted Passband: what is actually deployed

The record of THIS install, as of 2026-08-12. `SETUP.md` is the generic runbook
and explains why each piece exists; this file is the answers — which box, which
domain at which provider, which secret in which namespace — so nobody has to
reverse-engineer them at 3am.

Values are never in here. Names, namespaces and key names only.

## The box

| | |
|---|---|
| Name | `carrier` |
| Provider | Hetzner Cloud, CPX21 (**shared** vCPU: 3 vCPU / 4 GB / 80 GB root) |
| Region | US |
| OS | Ubuntu 26.04 |
| Public IP | `5.78.205.240` |
| Firewall | inbound 22, 80, 443 only |
| Backups | ON — 7 daily snapshots, **root disk only** (see "Backups" below) |
| SSH | `ssh carrier` (host alias in `~/.ssh/config`) |

Shared vCPU on purpose: tenant daemons are idle between syncs, and dedicated
(CCX) costs roughly triple for headroom this workload spends most of its life not
using. Scale vertically by resizing **CPU and RAM only** — a Hetzner resize that
grows the disk is a one-way door and blocks every later downsize.

## The volume

| | |
|---|---|
| Name | `passband-tenant-data` |
| Size | 50 GB, ext4 |
| Mount | on `carrier`, by-id in `/etc/fstab` with `nofail` |
| Delete protection | ON |
| Role | every tenant PVC — k3s local-path is repointed at it |

Grows online (expand in console → `resize2fs`). Never shrinks. **Not covered by
server backups.**

## k3s

Single node, installed as in `SETUP.md` §2, plus the storage path:

```
--secrets-encryption
--write-kubeconfig-mode=0600
--kube-apiserver-arg=feature-gates=UserNamespacesSupport=true
--kubelet-arg=feature-gates=UserNamespacesSupport=true
--default-local-storage-path <the volume mount>
```

`--secrets-encryption` is load-bearing: every tenant's age identity is a Secret
in this cluster's datastore. Verify with `k3s secrets-encrypt status`.

Namespaces: `warden` (the provisioner), `tenants` (everything per-tenant, Pod
Security Admission at `restricted`), `cert-manager`. Manifests applied from
`deploy/hosted/` in the numbered order.

## Railway

| | |
|---|---|
| Service | `control` — `squelch-control`, the signup plane |
| Build | `railway.control.toml`, set as the service's **Config-as-code file path** |
| Domain | `signup.passband.app` (CNAME to the Railway target) |
| Volume | mounted at `/data`; the store is `/data/control.sqlite3` |
| Health check | `GET /healthz` |

**The config file is what selects the image, not `RAILWAY_DOCKERFILE_PATH`.**
This service's first two deploys shipped the **relay** image with the variable
set correctly, because the root `railway.toml` is inherited by default and its
config-as-code outranks the variable. Any future service off this repo (the
broker) needs its own `railway.*.toml` the same way. `deploy/DEPLOY.md` §8 has
the long version.

Environment (names only — values are in Railway):
`SQUELCH_CONTROL_PUBLIC_URL`, `SQUELCH_CONTROL_BASE_DOMAIN`,
`SQUELCH_CONTROL_CLIENT_ID`, `SQUELCH_CONTROL_CLIENT_SECRET`,
`SQUELCH_CONTROL_COOKIE_KEY`, `SQUELCH_CONTROL_WARDEN_URL`,
`SQUELCH_CONTROL_WARDEN_TOKEN`, `SQUELCH_CONTROL_TRUSTED_PROXY_HOPS=1`,
plus the Bifrost pair — `SQUELCH_CONTROL_BIFROST_URL`,
`SQUELCH_CONTROL_BIFROST_ADMIN_TOKEN` — which is all-or-nothing: one
without the other is a refusal to boot. Four budget/model knobs ride on
top, all defaulted: `SQUELCH_CONTROL_LLM_BUDGET_USD` ($5) and
`SQUELCH_CONTROL_LLM_MODELS` for the triage key,
`SQUELCH_CONTROL_ASSISTANT_BUDGET_USD` ($10) and
`SQUELCH_CONTROL_ASSISTANT_MODELS` (haiku + opus) for the assistant key
the Passband app's relay chats spend against.
Full table: `squelch-control/README.md`.

### The LLM gateway

| | |
|---|---|
| Service | `bifrost` — [Bifrost](https://github.com/maximhq/bifrost) `v1.6.9`, via `Dockerfile.bifrost` |
| Build | `railway.bifrost.toml`, set as the service's **Config-as-code file path** — the same landmine as `control` above: the variable alone ships the relay image |
| Volume | mounted at `/app/data` — the governance state: virtual keys, per-tenant budgets, recorded spend |
| Health check | none, deliberately: Bifrost's `GET /health` sits behind its session auth once auth is on, and Railway's probe cannot present credentials |

Environment: `ANTHROPIC_API_KEY` — **the only place our real Anthropic key
lives, anywhere** — plus `APP_HOST` and `APP_PORT`.

Tenant daemons reach it over public 443, which their egress policy already
allows; there is no cluster-side plumbing to the gateway at all. Each daemon
presents its own **virtual key**, minted by the control plane at signup with a
monthly dollar budget and installed through the warden as that tenant's
`<label>-llm` Secret. Bifrost swaps in the real key, meters the spend on its
volume, and refuses a tenant that has blown its budget.

The cluster half of the feature is the `SQUELCH_WARDEN_LLM_*` block in
`20-warden.yaml`: `SQUELCH_WARDEN_LLM_BASE_URL` (the feature gate — the warden
refuses to boot if any of the rest are set without it),
`SQUELCH_WARDEN_LLM_STAGE1_MODEL`, `SQUELCH_WARDEN_LLM_STAGE2_MODEL`,
`SQUELCH_WARDEN_LLM_STAGE1_DAILY_CAP`, `SQUELCH_WARDEN_LLM_STAGE2_DAILY_CAP`.
Install procedure: `SETUP.md` → "LLM triage through the gateway".

> **History.** Before the gateway, keyed tenants ran on a shared
> `anthropic-api-key` Secret in ns `tenants` — the pre-gateway bridge, removed
> from the warden 2026-08-13. Removing the code touches no tenant that already
> exists (see "Shipping a tenant-shape change" below): a tenant provisioned
> while the bridge existed still carries the `ANTHROPIC_API_KEY` env from its
> old rendered spec, and with the gateway base URL alongside it the daemon
> resolves that raw key and every Stage-2 call 401s against the gateway —
> failing, not idle.
>
> **Which command clears it depends on who owns the field, and the two answers
> are not interchangeable.** `squelch-control llm mint <label>` re-applies the
> whole Deployment from today's render, so it strips a legacy env that the
> WARDEN put there — that field is one the warden used to declare and has
> stopped declaring, which is exactly what a server-side apply removes. It does
> nothing at all to an env that was HAND-PATCHED on with `kubectl set env`:
> that field belongs to `kubectl`'s field manager, and an apply never removes a
> field it does not declare, no matter how many times it runs. For that one the
> command is `squelch-control reconcile <label>`, which deletes and recreates
> the Deployment when a foreign manager owns anything on it.
>
> The roller closes the first case without being asked: an env the warden used
> to declare and no longer does is exactly the drift a roll converges, and the
> apply strips it. The hand-patched one it will not touch, and reports as a
> foreign-drift skip until a person runs `reconcile`.
>
> `squelch-control drift <label>` tells you which case you are in before you
> pick, and confirms afterwards. Then
> `kubectl -n tenants get deploy -o yaml | grep -c ANTHROPIC_API_KEY` should say
> 0, and the job finishes by deleting the Secret itself:
> `kubectl -n tenants delete secret anthropic-api-key`.

## DNS

Registrar and DNS provider are different questions, and here they differ per
zone:

| Zone | Registrar | DNS hosted at | Why |
|---|---|---|---|
| `passband.email` (tenants ONLY) | Namecheap | **Cloudflare** (free) | the wildcard cert needs DNS-01, and cert-manager has no Namecheap solver |
| `passband.app` (product + internal) | Namecheap | Namecheap | single-host certs issue over HTTP-01; no DNS credential needed |

Records:

| Name | Type | Value |
|---|---|---|
| `*.passband.email` | A | `5.78.205.240` — **DNS-only / grey cloud** |
| `warden.passband.app` | A | `5.78.205.240` |
| `signup.passband.app` | CNAME | the Railway target |

**The wildcard must never be proxied.** Cloudflare's orange cloud would
terminate TLS in front of every tenant mailbox and fight cert-manager's
certificate. The dashboard defaults new A records to proxied, so check it after
every zone edit.

Certificates: `passband-wildcard` (ns `tenants`, DNS-01 via Cloudflare) and
`warden` (ns `warden`, HTTP-01 catch-all). Both from the `letsencrypt`
ClusterIssuer in `40-wildcard-certificate.yaml`.

## Images

Every `daemon-X.Y.Z` tag builds all three from
`.github/workflows/release-daemon.yml` and pushes them to GHCR. The image tag is
the git tag verbatim — `ghcr.io/braelyn-ai/squelchd:daemon-0.0.1`, plus a moving
`latest`. There are no bare numeric tags any more, deliberately: GHCR tags are
mutable and this node pulls `IfNotPresent`, so a recycled number would pin a
stale image with nothing to show for it. The old numeric tags (`0.2.x`,
`v0.2.6`) are frozen history; anything still pinned to one keeps working until
it is repointed.

| Image | Arch | Built by |
|---|---|---|
| `ghcr.io/braelyn-ai/squelchd:<tag>` | amd64 + arm64 | `squelchd-image`; the Dockerfile cross-compiles, so no emulation |
| `ghcr.io/braelyn-ai/squelch-warden:<tag>` | **amd64 only** | `warden-image`; builds natively, and emulating the ONNX build risks the six-hour job ceiling |
| `ghcr.io/braelyn-ai/squelch-control:<tag>` | **amd64 only** | `control-image`; same reason. Railway still builds its own copy from source, so this tag is for reproducibility and for running the plane elsewhere |

`carrier` is amd64, so the two amd64-only images cover everything hosted
actually runs. Moving the hosted planes to an arm box means giving those
Dockerfiles a cross-compiling shape first, or getting a native arm runner — not
turning binfmt back on.

> **The registry gap is closed in CI, and not yet on this node.** The warden
> image running here is still the one built by hand on `carrier` and loaded
> straight into containerd:
>
> ```sh
> docker build -f Dockerfile.warden -t ghcr.io/braelyn-ai/squelch-warden:v0.2.0 .
> docker save ghcr.io/braelyn-ai/squelch-warden:v0.2.0 | k3s ctr images import -
> ```
>
> That tag carries a GHCR name nothing has ever pushed, and adding the CI job
> does not retroactively publish it — `v0.2.0` is also from the retired
> numbering, so nothing ever will. So: do not delete this image from
> containerd, and do not assume `imagePullPolicy` will save you, until a
> `daemon-*` tag is cut and `20-warden.yaml` is repointed at a tag the registry
> actually has. From that point on, replacing this node is a pull.

The squelchd and warden tags are both pinned in `20-warden.yaml`, at
`daemon-*` tags, and again in `90-warden-roller.yaml`, which renders tenants
with the same code and must therefore carry the same two values. The warden
refuses to start with an untagged tenant image.

## Shipping a tenant-shape change

**Rolling out a new warden does not change tenants that already exist.** Each
tenant's Ingress, NetworkPolicy, Service and Deployment are written once, at
provision time, and the warden is not a controller: kube reconciles a tenant's
pod against those objects, and nothing reconciles those objects against the
warden's current code until somebody asks it to.

So anything that changes the SHAPE of a tenant — a new Ingress path prefix, a new
environment variable in the pod (`SQUELCH_CONSOLE_SSO_URL` and the LLM gateway
block — `SQUELCH_ANTHROPIC_BASE_URL`, `SQUELCH_STAGE2_PROVIDER`,
`SQUELCH_STAGE2_API_KEY`, the pod-side names the warden's `SQUELCH_WARDEN_LLM_*`
config renders to — are both this), a changed NetworkPolicy peer, new resource
bounds — lands on new signups and on nobody else, silently. Asking is two
commands, run where the tenant list is:

```sh
railway ssh --service control
squelch-control drift                 # every tenant; exit 1 if anything has drifted
squelch-control drift alice           # one tenant, in full
squelch-control reconcile alice       # put that tenant back on today's render
```

`drift` is read-only. Per tenant it asks the warden two independent questions —
which OTHER field managers own part of that Deployment, and what an apply of
today's render would change — and the second one is a `dryRun` apply the API
server merges and throws away. Nothing rolls, nothing is stored.

`reconcile` is the fix: it re-applies that tenant's PVC, NetworkPolicy, Service,
Deployment and Ingress from current code, in provision order, and does not answer
until a pod is Ready again. What it costs depends on what it finds. An Ingress or
NetworkPolicy change lands with nothing restarting. A pod-shape change (env,
image, resources) rolls the pod the way any Deployment update does. And if a
foreign field manager owns anything on the Deployment, it deletes the Deployment,
waits for the old pod to let go of the `ReadWriteOnce` volume, and applies a
fresh one — that mailbox is down for the window, and it is the only way those
fields ever go away (see the open item below for why).

**One tenant at a time, verified.** That is the whole discipline, and the roller
below is it done by a timer that does not get bored on tenant nine: one
reconcile, one finished rollout, then the next, and a stop at the first tenant
that does not come back. Driving it by hand is still the right move when you
want to choose the ORDER — reconcile the least important label, watch the pod
come back, run `drift` on it again — and fleet `drift` tells you how long the
list is without touching anything.

**A reconcile that died in its own delete/apply window** leaves the tenant
reading `stopped`, because for that moment it has no Deployment. Wait for the
old pod to finish terminating and run the same `reconcile` again: a surviving
Service is how the warden tells an interrupted reconcile from a cancelled
account, so it resumes rather than refusing. Nothing was lost — the volume, the
identity and the sealed credential never moved.

**The fallback, for what reconcile refuses.** A `pending` tenant, and a
`stopped` one that was genuinely cancelled (its Service is gone too), come back
as `409 not_reconcilable`; starting either back up is a different transition,
not a shape repair.

Reopening one is **re-consent, not a re-`PUT`**. Nothing outside the tenant's
own Secret holds a copy of that ciphertext: `squelch-control`'s schema carries
no tokens and no ciphertext by design, and the refresh token it seals exists in
memory for the length of one signup request. So the person signs in again, the
control plane seals a fresh credential to the recipient the warden still holds,
and `PUT /v1/tenants/{label}/credentials` rebuilds every object from today's
code. The volume, the identity and the old sealed blob were never touched; the
mailbox is down for the length of a provision.

> **Standing rule: no hand edits on a tenant Deployment. Ever.** Not
> `kubectl set env`, not `kubectl edit`, not a client-side `kubectl apply`. An
> env change is a warden config or manifest change, a warden release, and then
> `reconcile` per tenant. The reason is that server-side apply owns FIELDS: a
> field the warden does not declare belongs to whoever wrote it, and every
> warden apply afterwards converges politely around it forever. In 2026-08 a
> `set env` put a Secret reference on the seed init container of a live tenant;
> it survived weeks of applies invisibly and then detonated as
> `Init:CreateContainerConfigError` on an unrelated rollout, once the Secret it
> named was gone. That state is exactly what the `foreign` half of a drift
> report exists to surface, and the delete-and-recreate is the only thing that
> clears it.

## Rolling the daemon image

Three steps, and only the middle one is a decision.

1. **CI publishes.** A `daemon-X.Y.Z` tag builds
   `ghcr.io/braelyn-ai/squelchd:daemon-X.Y.Z` and pushes it to GHCR
   (`docs/RELEASING.md`, surface 1). Nothing on this box moves, and no tenant is
   affected: the registry holding an image is not a deploy.
2. **A human bumps the pin and applies.** `SQUELCH_WARDEN_IMAGE` in
   `20-warden.yaml` **and its copy in `90-warden-roller.yaml`** — two halves of
   one setting, and a difference between them is the flap described in that
   file's env-block comment. `kubectl apply -f` both. This is the deliberate
   decision to roll the fleet, and it is the only step with a person in it.
3. **The roller converges the fleet.** The CronJob in `90-warden-roller.yaml`
   runs `squelch-warden roll` every 15 minutes, on the warden's image, under the
   warden's own ServiceAccount, with the warden's own environment. It lists
   every tenant in the cluster, reconciles the ones whose live Deployment no
   longer matches today's render, waits for each rollout to actually finish
   before touching the next tenant, and stops at the first one that does not
   come back.

Nothing in that chain reaches in from outside. It is the warden's binary calling
its own library code on the box: no credential leaves the cluster to trigger a
roll, GitHub Actions has no power over production, and the control plane's
bearer token buys no part of it.

**What it costs a tenant: one pod restart.** Zero downtime per tenant is not on
offer and never will be — the strategy is `Recreate`, the volume is
`ReadWriteOnce`, and the store is one SQLite file, so two daemons on one mailbox
would corrupt it. The tenant's console and API are unreachable for the length of
one pod cycle.

**What it costs the fleet: nothing.** One tenant is down at a time, and only
while its replacement comes up. There is no moment where the fleet is down, and
a render that cannot come up costs exactly one tenant before the run halts.

**No mail is lost, and that is not a hedge.** Gmail holds the mail; the tenant's
store is a local index of it. A daemon that is restarted mid-sync resumes on its
next tick and re-fetches whatever it had not finished, and the volume, the
identity Secret and the sealed credential are not touched by a roll at all.

The run's answer is its exit code, which is also the Job's status:

| Exit | Means | What to do |
|---|---|---|
| 0 | The fleet is on today's render and serving it. A run with nothing to do is this. | Nothing. |
| 1 | Not converged, in any of its forms: it halted on a tenant that did not come back; it stopped before applying anything because a tenant already carries today's render and is not serving it; a tenant has no workload behind a live Service; it never started (refused config, unreachable API server); or `--dry-run` found work to do. | Read the log, find the named label, suspend the CronJob while you work. Everything after that tenant in label order was not touched. |
| 2 | Converged, and at least one tenant was skipped for foreign drift. | Below. Nothing is broken; nothing fixes itself either. |
| 64 | The Job's argument list is wrong. | Fix `args:` in `90-warden-roller.yaml`. |

Anything other than 0 marks the Job **failed**, which is deliberate: a fleet that
is not converged should be visible as a failure and not as a line in a log
nobody reads.

```sh
kubectl -n warden get cronjob squelch-warden-roll         # last schedule, suspended or not
kubectl -n warden get jobs                                # one row per run, 3 kept + 5 failed
kubectl -n warden logs job/<name>                         # per-tenant lines, then the summary
kubectl -n warden create job --from=cronjob/squelch-warden-roll roll-now   # do not wait
kubectl -n warden patch cronjob squelch-warden-roll -p '{"spec":{"suspend":true}}'
```

**A run that stops before applying anything is the halt doing its job across
runs.** The roller reads every tenant before it writes to any of them, and a
tenant that carries today's render and is not serving it stops the run where it
stands: that render was applied there and the mailbox did not come back, so no
other mailbox is getting it. The summary says `HALTED before applying anything`
and names the tenant. Look at that one tenant first (`kubectl -n tenants logs
deploy/<label>`, `squelch-control drift <label>`); the fleet is exactly as the
previous run left it, and it stays that way until the tenant is serving again or
you suspend the CronJob. A tenant that is down for its own reasons blocks a roll
the same way, which is deliberate: the fleet is not rolled while a mailbox is
down.

**A tenant reported as DOWN with no workload** is a reconcile that did not
finish: its Service, volume and sealed credential are standing and only the
Deployment is missing. The roller will not finish somebody else's half-done
repair unattended, so it names the tenant and leaves it. `squelch-control
reconcile <label>` is the finish.

**A foreign-drift skip (exit 2) is a page for a person, not a bug.** The roller
refuses to repair a Deployment another field manager owns fields on, because the
only repair server-side apply allows is deleting the Deployment and applying a
fresh one — that takes somebody's live mailbox down to remove a field a human
put there on purpose, and a timer must not make that call. The named tenant
stays on its old render, which is the safe half. Run
`squelch-control drift <label>` to see who owns what, and
`squelch-control reconcile <label>` when you are ready for that mailbox to be
down for a pod cycle. Until you do, every run will report it again.

**The one way the fleet still moves without anyone asking.**
`PUT /v1/tenants/{label}/llm-key` — which is what `squelch-control llm mint`
calls — re-renders and re-applies that tenant's WHOLE Deployment, because the key
rides on the pod template as a hash. So rotating one tenant's virtual key
silently moves that tenant onto whatever `SQUELCH_WARDEN_IMAGE` is pinned today,
with no rollout decision behind it. The roller makes that self-correcting rather
than permanent (the rest of the fleet is converging on that same pin anyway), but
it is worth knowing when a drift run shows a fleet you did not expect: the
tenants reading as current may be the ones whose keys were rotated last week
rather than the ones you rolled.

## Cluster secrets

Inventory only. Nothing here should ever be printed into a document, a ticket or
a chat window.

| Namespace | Secret | Keys | Created by |
|---|---|---|---|
| `warden` | `squelch-warden` | `token` | operator (`openssl rand -base64 32`); the same value is `SQUELCH_CONTROL_WARDEN_TOKEN` on Railway |
| `tenants` | `google-oauth-client` | `client_id`, `client_secret` | operator; the confidential **web** client, the same one the control plane consents with |
| `cert-manager` | `acme-dns-token` | `api-token` | operator; Cloudflare token scoped to the `passband.email` zone, DNS:Edit |

Managed by the cluster, listed so an inventory sweep does not mistake them for
strays: `passband-wildcard-tls` (ns `tenants`), `warden-tls` (ns `warden`),
`letsencrypt-account-key` (ns `cert-manager`), and per tenant
`<label>-identity` + `<label>-credential` + `<label>-llm` (ns `tenants`). The
`-llm` one is the tenant's Bifrost virtual key (key `api-key`), written by the
warden when the control plane mints it; it is absent for a tenant that signed
up while the gateway was down (the pod boots without it and triages on rules —
backfill is `squelch-control llm mint <label>`), and unlike the other two it is
replaceable: revoke and re-mint, nothing is lost.

The per-tenant identity Secrets are the only irreplaceable thing in this
cluster. Losing one is a re-consent for that tenant; there is no escrow, by
design.

## Monitoring

The rule: nothing that judges carrier's health lives on carrier. Full design:
`deploy/monitoring/README.md`.

- **On carrier**, ns `monitoring` (from `80-monitoring.yaml`): node-exporter,
  kube-state-metrics, and `prometheus-agent`, which scrapes locally and pushes
  via `remote_write` — outbound only, the firewall stays 22/80/443.
- **On Railway**, three more services in the same project, each with its own
  config-as-code file (the §8 lesson): `prometheus` (receiver + 30d storage,
  volume at `/prometheus`, `prometheus-production-2d43.up.railway.app`, basic
  auth on every route), `blackbox` (probes signup/warden/landing from outside;
  **no public domain, ever**), `grafana` (volume at `/var/lib/grafana`,
  `grafana-production-00b9.up.railway.app`, the `passband-health` dashboard
  provisioned from the repo — edit the JSON and redeploy, never the UI).
- Secrets: `monitoring`/`remote-write-auth` (key `password`) on carrier; the
  same value as `PROM_REMOTE_WRITE_PASSWORD` + its bcrypt `PROM_WEB_BCRYPT`
  on the prometheus service; same value again as `PROM_PASSWORD` plus
  `GF_SECURITY_ADMIN_PASSWORD` on the grafana service.
- **Inside the daemon**: every tenant pod runs with
  `SQUELCH_METRICS_BIND=0.0.0.0:9464` and serves `squelchd_*` Prometheus text on
  container port `metrics` (sync timestamps, Gmail API errors by kind, LLM
  spend, store size, triage verdicts). `prometheus-agent` keeps on
  `app.kubernetes.io/name=squelchd` in `tenants` plus the port name, and
  relabels `app.kubernetes.io/instance` to `tenant`.
- That listener is unauthenticated, so its reachability is the whole control:
  the per-tenant NetworkPolicy admits only the `monitoring` namespace's
  `app.kubernetes.io/name=prometheus-agent` pod, only to 9464. The port is on
  the pod and on nothing that publishes it (absent from the tenant Service and
  from its Ingress), and 8848 stays sealed to Traefik. Alert on
  `time() - squelchd_sync_last_success_timestamp_seconds > 900`: it is the only
  signal that separates a healthy pod from one that stopped syncing days ago.

## Invites

Minted on the control service, not locally — the store is on its volume:

```sh
railway ssh --service control
squelch-control invite issue --count 5      # printed ONCE, on stdout
squelch-control invite list                 # ids and status, never codes
squelch-control invite revoke <id>
squelch-control tenants
```

Codes are `XXXX-XXXX-XXXX-XXXX`, single use, 30 days. Stored as SHA-256 only, so
a lost code is re-issued, never recovered.

**Or approve from `/admin`.** With the waitlist trio set on the control service
(`SQUELCH_CONTROL_ADMIN_TOKEN`, `SQUELCH_CONTROL_RESEND_API_KEY`,
`SQUELCH_CONTROL_INVITE_FROM`, and the sending domain **verified at Resend**),
`https://signup.passband.app/admin` lists everybody still waiting, oldest first,
with the most recent approvals under them as history, and one button mints a code
and emails it. That is the everyday path. The CLI
above stays the break-glass one: it needs no browser, no Resend, and no
`SQUELCH_CONTROL_ADMIN_TOKEN`, so it still issues codes when the mailer is down.

A send the provider refused leaves the row approved and badged "email not sent"
with a button beside it. Pressing it **revokes the code nobody received and
mints a new one**, because only the hash was kept and nothing can read a code
back out. Same button, quieter, on a row whose invite was delivered and lost.
One thing the button will not do is take a code out from under somebody: a row
whose invite is being redeemed right now says so and changes nothing, because
that person has already granted Google consent they cannot grant twice.

Rotating `SQUELCH_CONTROL_ADMIN_TOKEN` signs every admin session out on its next
request, so a token you think leaked is a token you can simply change.

With the trio unset the waitlist and `/admin` are not mounted at all: an
unconfigured deployment answers 404 there, not 403.

## Backups

Two mechanisms, split by which disk they cover:

- **Hetzner server backups: ON, 7 daily, root disk only.** Covers the k3s
  datastore, therefore every tenant's identity Secret. Load-bearing, and the
  only thing that covers the Secrets at all.
- **Litestream → Cloudflare R2: ON** (built 2026-08-10; retargeted to the 0.3.13
  line for client-side encryption 2026-08-11). Covers the
  `passband-tenant-data` volume, which Hetzner's snapshots do not.

| | |
|---|---|
| What streams | each tenant's SQLite, continuously, from the WAL, **age-encrypted before upload** |
| From | `/mnt/tenant-data/pvc-<pv-uid>_tenants_<label>-data/squelch.db` |
| To | `s3://passband-tenant-backups/tenants/<label>/store.db` (R2) |
| How | **one** systemd service on `carrier` (`litestream.service`), not a sidecar per pod — see below |
| Discovery | `litestream-config.timer` → `/usr/local/bin/litestream-sync-config.sh` re-renders `/etc/litestream.yml` from the PVC directories every 2 min |
| Litestream state | `/var/lib/litestream/<label>` (`meta-path`) — off the tenant volume, so a tenant pod cannot touch its own backup bookkeeping |
| Units | `/etc/systemd/system/{litestream,litestream-config}.service`, `litestream-config.timer`, from `deploy/hosted/litestream/` |
| Version | litestream **v0.3.13**, upstream `.deb`, `apt-mark hold` |

**Why 0.3.13 and not the current line.** 0.3.x is the last release series with
client-side age encryption; 0.5.x removed it and refuses to start on a config
that asks for it. Encrypting the copy handed to a third party outweighs being on
the maintained line, given the payload is the full text of other people's mail.
0.3.13 shipped October 2023 and gets no fixes — the mitigation is running the
restore drill on a schedule. The two lines' config schemas also differ in a way
that **fails silently**: 0.3 wants `replicas:` (a list), 0.5 wants `replica:`,
and 0.3.13 accepts the 0.5 shape with zero replicas attached and no error. The
health check that catches it is `litestream databases` printing `s3` in the
replicas column.

**Why host-level and not a sidecar.** A sidecar needs the R2 write credential
inside every tenant pod, and S3-style credentials cannot be scoped to
"your own prefix, append only" — so one compromised tenant could delete the
whole fleet's backups. Host-level keeps that credential with root, which already
owns the disk.

### Key custody

| Secret | Lives | If lost |
|---|---|---|
| R2 token (Object Read & Write, scoped to `passband-tenant-backups`) | `/etc/litestream/env` on `carrier`, root:root 0600. Never in git, never in a Secret | mint a new one in the Cloudflare dashboard; nothing is lost |
| **backup age recipient** (public) | `LITESTREAM_AGE_RECIPIENT` in `/etc/litestream/env` | re-derive with `age-keygen -y` from the identity |
| **backup age identity** (private) | `/etc/litestream/backup-age.key` on `carrier`, root:root 0600, **and the password manager**. Read by nothing automatically; exported by hand for the minutes a restore takes | **unrecoverable, fleet-wide.** Every backup in R2 becomes permanently unreadable, and nothing tells you until you try to restore |
| tenant identity Secrets (`<label>-identity`, ns `tenants`) | k3s datastore, encrypted at rest. Covered by the Hetzner snapshot only | **unrecoverable.** Every affected tenant re-consents with Google. No escrow, by design |

**Backups are age-encrypted client-side.** Every snapshot and WAL segment is
sealed to the recipient before it leaves the box, so R2 objects begin
`age-encryption.org/v1` and Cloudflare cannot read a tenant's mail index. The
replicating daemon holds only the public half and therefore cannot open a single
backup it writes; restores load the identity into one operator shell. A
compromised bucket on its own discloses nothing. `SETUP.md` §11 step 3 has the
generation procedure and the custody warning.

**The custody rule this creates.** The backup age identity now sits in the same
tier as the tenant identity Secrets: kilobytes, off-box, in the password manager,
or the thing they protect is gone. It is not in the bucket it opens, and it never
will be.

`SETUP.md` → "Backups today, stated honestly" has the failure-mode table, and
§11.6/§11.7 have the per-tenant and full-DR restore drills. The first line of the
DR drill is `systemctl mask litestream.service litestream-config.timer` — on a
fresh box, a re-provisioned tenant creates an empty database that litestream
would happily stream over the real history.

## Open items

- **A scheduled restore drill.** 0.3.13 is an unmaintained pin (see above) and a
  backup nobody has restored is a hypothesis. `SETUP.md` §11.6 is the drill; it
  needs a calendar entry, not just a runbook entry.
- **Off-box schedule for the two irreplaceable keys** — the tenant identity
  Secrets and `/etc/litestream/backup-age.key`. Both are still covered only by
  Hetzner's root-disk snapshot and by whoever remembers to run
  `kubectl -n tenants get secret -o yaml` and copy the key file. Litestream does
  not touch Secrets and never will, and it certainly does not back up its own
  key into the bucket that key opens.
- **Cut a `daemon-*` tag that publishes the warden** — the release workflow now
  has the job (see "Images"), but this node still runs the hand-built image. The
  item closes when `20-warden.yaml` points at a tag GHCR actually holds.
- **Nothing alerts on a roll that did not converge.** The converging half is
  covered: the roller (see "Rolling the daemon image") walks the fleet every 15
  minutes and fixes what it can. What is missing is anybody finding out when it
  cannot. A halt or a foreign-drift skip is a failed Job in ns `warden` and
  nothing else — no email, no dashboard panel, and `failedJobsHistoryLimit: 5`
  runs of head start before the evidence rotates away. kube-state-metrics is
  already scraped off this box, so the alert is
  `kube_job_status_failed{namespace="warden"} > 0` plus a panel; it wants
  writing, not inventing.

  Two facts about repairing a foreign-owned field worth keeping written down,
  because the shape of the fix is not the obvious one. **Re-running phase two's server-side applies
  does NOT purge a foreign-owned field.** SSA removes only fields the applier
  itself declares and has stopped declaring; a field belonging to another
  manager is never in that set, so `--force-conflicts` and twenty repeat applies
  leave it exactly where it is. The only honest purge is deleting the Deployment
  and applying a fresh one, whose ownership ledger starts empty — which is what
  `reconcile` does when, and only when, a foreign manager is found, and it is
  why that path rolls the pod (and waits for the old one to release the RWO
  volume first) when an ordinary converge would not have.
- **`/mcp` bearer auth** — the hosted MVP routes around it (tenant Ingresses
  publish `/client`, `/console` and `/t` only). Real auth is required before the
  agent door is ever served from the internet, and until then the console's MCP
  section says so instead of offering a URL that 404s.
- **Google verification + CASA Tier 2** — the restricted-scope cap is 100 users.
  This gates user 101 and has the longest lead time of anything on this list.
- **Public Suffix List entry for `passband.email`** — a PR to `publicsuffix/list`
  (private section, plus a `_psl` TXT record) makes browsers treat every tenant
  subdomain as its own registrable domain: no cross-tenant cookies, per-tenant
  SameSite boundaries — the `github.io` treatment. Low urgency while tenant
  vhosts serve only the bearer-authed API, but file it well before any
  browser-facing surface: propagation into shipped browsers takes weeks and
  listing is effectively permanent.
- **USPTO trademark search** — a collision search found no software product
  named Passband; a proper search is still owed before paperwork is filed under
  the name (`docs/HOSTED.md`, "Naming").
