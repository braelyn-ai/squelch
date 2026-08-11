# Hosted Passband: what is actually deployed

The record of THIS install, as of 2026-08-10. `SETUP.md` is the generic runbook
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
`SQUELCH_CONTROL_WARDEN_TOKEN`, `SQUELCH_CONTROL_TRUSTED_PROXY_HOPS=1`.
Full table: `squelch-control/README.md`.

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

| Image | Built by | Where it lives |
|---|---|---|
| `ghcr.io/braelyn-ai/squelchd:v0.2.0` | CI, `.github/workflows/release.yml` on every `v*` tag, multi-arch amd64+arm64 | GHCR |
| `ghcr.io/braelyn-ai/squelch-warden:v0.2.0` | **by hand, on `carrier`** | nowhere — only this node's containerd |

> **REGISTRY GAP.** The warden image was built natively on the box and loaded
> straight into containerd:
>
> ```sh
> docker build -f Dockerfile.warden -t ghcr.io/braelyn-ai/squelch-warden:v0.2.0 .
> docker save ghcr.io/braelyn-ai/squelch-warden:v0.2.0 | k3s ctr images import -
> ```
>
> It carries a GHCR name it has never been pushed to. Consequences, in order of
> how much they will hurt: reimaging or replacing this node loses the image and
> the rebuild is manual; nothing can roll back to a previous warden; and the tag
> is a promise no registry is keeping. The fix is a warden job in the release
> workflow next to the squelchd one. Until then, do not delete this image from
> containerd and do not assume `imagePullPolicy` will save you.

Both tags are pinned in `20-warden.yaml`. The warden refuses to start with an
untagged tenant image.

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
`<label>-identity` + `<label>-credential` (ns `tenants`).

The per-tenant identity Secrets are the only irreplaceable thing in this
cluster. Losing one is a re-consent for that tenant; there is no escrow, by
design.

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

- Hetzner server backups: ON, 7 daily, **root disk only**. That covers the k3s
  datastore, therefore every tenant's identity Secret. Load-bearing.
- The `passband-tenant-data` volume is **not** covered. Mailbox loss = every
  tenant re-syncs from Gmail. Slow, not fatal.
- Litestream is **not** wired up. Still owed.

`SETUP.md` → "Backups today, stated honestly" has the failure-mode table.

## Open items

- **Litestream** — promised in `docs/HOSTED.md` "from day one", never built. The
  only thing standing between a bad day and a fleet-wide re-consent is a
  root-disk snapshot.
- **Warden image in CI** — a release-workflow job that publishes
  `squelch-warden` the way `squelchd` is already published. Closes the registry
  gap above.
- **`/mcp` bearer auth** — the hosted MVP routes around it (tenant Ingresses
  publish `/client` and `/t` only). Real auth is required before the agent door
  is ever served from the internet.
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
