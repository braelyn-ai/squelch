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
plus the Bifrost trio — `SQUELCH_CONTROL_BIFROST_URL`,
`SQUELCH_CONTROL_BIFROST_ADMIN_TOKEN`, `SQUELCH_CONTROL_LLM_BUDGET_USD` —
which is all-or-nothing: a partial trio is a refusal to boot.
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

> **The stopgap this supersedes.** Before the gateway, keyed tenants ran on a
> shared `anthropic-api-key` Secret in ns `tenants` with the env hand-patched
> onto their Deployments — the real key, inside tenant pods, exactly what this
> design exists to avoid. Both halves get removed during the gateway rollout:
> delete the Secret, then push each tenant's virtual key through the warden
> (`squelch-control llm mint <label>`). The llm-key install re-applies the
> whole Deployment from the warden's rendered spec (server-side apply), so any
> hand-patched Deployment converges back to spec in the same stroke — no
> manual un-patching, and a hand-patch that lingers is a rollout that skipped
> that tenant.

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
`daemon-*` tags. The warden refuses to start with an untagged tenant image.

## Shipping a tenant-shape change (there is no reconcile)

**Rolling out a new warden does not change tenants that already exist.** Each
tenant's Ingress, NetworkPolicy, Service and Deployment are written once, at
provision time, and the warden never looks at them again. Kube reconciles a
tenant's pod against those objects; nothing reconciles those objects against the
warden's current code.

So anything that changes the SHAPE of a tenant — a new Ingress path prefix, a new
environment variable in the pod (`SQUELCH_CONSOLE_SSO_URL` and `ANTHROPIC_API_KEY`
are both this), a changed NetworkPolicy peer, new resource bounds — lands on new
signups and on nobody else, silently. Check what a live tenant actually has
before assuming a deploy reached it:

```sh
kubectl -n tenants get ingress alice -o yaml
kubectl -n tenants get deploy alice -o jsonpath='{.spec.template.spec.containers[0].env}'
```

Two ways to catch an existing tenant up, both manual, and `squelch-warden/README.md`
("What does not reconcile") has the full Ingress body to apply:

1. **`kubectl apply` the one object**, for an Ingress or a NetworkPolicy, which
   needs no restart. It means hand-writing YAML with a tenant label in it, which
   the warden itself is forbidden to do; on an operator's terminal, for a label
   that is already provisioned, that is the accepted workaround.
2. **`DELETE` then `PUT` the credential again** through the warden API, for
   anything that touches the pod. `DELETE /v1/tenants/{label}` keeps both Secrets
   and the volume, and the `PUT` rebuilds every object from today's code. It
   recycles the pod, so that mailbox is down for the length of a provision, and
   the control plane has to still hold the sealed blob to re-send.

Until the reconcile route below exists, a tenant-shape change is a manual pass
over the tenant list, and it is worth doing at the moment it ships rather than
discovering months later that half the fleet is a shape nobody remembers.

## Cluster secrets

Inventory only. Nothing here should ever be printed into a document, a ticket or
a chat window.

| Namespace | Secret | Keys | Created by |
|---|---|---|---|
| `warden` | `squelch-warden` | `token` | operator (`openssl rand -base64 32`); the same value is `SQUELCH_CONTROL_WARDEN_TOKEN` on Railway |
| `tenants` | `google-oauth-client` | `client_id`, `client_secret` | operator; the confidential **web** client, the same one the control plane consents with |
| `tenants` | `anthropic-api-key` | `ANTHROPIC_API_KEY` | operator; one shared key every tenant daemon runs Stage-2 triage with. Referenced `optional: true`, so its absence is heuristic-only triage rather than a fleet that will not start. The bridge until the relay ships (issue #33) |
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
- **Tenant reconcile: `POST /v1/tenants/{label}/reconcile`.** The warden
  re-applies the typed objects for one already-provisioned tenant, on demand,
  from its current code — the same server-side applies phase two runs, minus the
  identity mint and the credential seal, reading the sealed blob from the Secret
  it already holds rather than taking one on the wire. On demand and per label,
  not a controller: a loop that re-applies to every tenant is a loop that can
  take the whole fleet down on one bad deploy, whereas a route the operator
  drives can be run against one tenant, verified, and then walked across the
  list. It needs the same 404/409 shape the other routes have, an answer for the
  case where the change rolls the pod (an env or image change does, an Ingress or
  NetworkPolicy change does not), and it should refuse a tenant that is not
  `active`. **Needed the first time any tenant-shape change ships after tenants
  exist** — which is now: `SQUELCH_CONSOLE_SSO_URL` and the optional
  `ANTHROPIC_API_KEY` reference both reach new tenants only, and "Shipping a
  tenant-shape change" above is the manual stand-in.
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
