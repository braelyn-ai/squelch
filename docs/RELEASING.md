# Releasing

The system has four release surfaces that move independently, on different
triggers, with different blast radii. A "release" is usually one of these, not
all of them; know which one you are doing before you start.

| Surface | Trigger | What ships | Who is affected |
|---|---|---|---|
| Server images | push tag `v*` | `ghcr.io/braelyn-ai/{squelchd,squelch-warden,squelch-control}` | nobody, until a deploy points at the tag |
| Hosted carrier | manual ops on `carrier` | warden + tenant pods | every hosted tenant |
| Passband app | push tag `passband-v*` | GitHub Release + appcast + Homebrew cask | every Mac user, via Sparkle |
| Railway services | git push to `main`, or `railway up` | control, landing site, relay, monitoring | signup, passband.app, push relay |

Existing per-surface runbooks this document links instead of duplicating:
[`passband/RELEASING.md`](../passband/RELEASING.md) (the app),
[`deploy/hosted/SETUP.md`](../deploy/hosted/SETUP.md) (hosted, generic),
[`deploy/hosted/PRODUCTION.md`](../deploy/hosted/PRODUCTION.md) (this install),
[`deploy/DOCKER.md`](../deploy/DOCKER.md) (self-host containers),
[`deploy/DEPLOY.md`](../deploy/DEPLOY.md) (bare metal),
[`deploy/monitoring/README.md`](../deploy/monitoring/README.md) (dashboards).

## Version map

There is no single version. Keep the map in your head or you will ship the
wrong number somewhere:

- **Rust workspace** — `Cargo.toml` `[workspace.package] version`, inherited by
  every crate. This is what `squelchd_build_info{version=...}` reports on the
  dashboard, so bump it with the tag or Grafana cannot tell your releases
  apart. Nothing enforces this; it is on you.
- **Image tags** — the git tag itself. `v0.3.0` publishes `0.3.0`, `0.3`,
  `v0.3.0`, and moves `latest` (verified against GHCR; self-host docs depend
  on `latest`).
- **Passband** — `passband/VERSION` is the marketing version;
  `project.yml` `MARKETING_VERSION` must mirror it (release.sh preflights
  this; CI does NOT, so check it before tagging). The Sparkle ordering key is
  the BUILD number (`git rev-list --count HEAD`), which is why the marketing
  version going backwards once (1.0.0 → 0.0.2) did not strand updaters. Do
  not rely on that accident twice.
- **Deploy pins** — `deploy/hosted/20-warden.yaml` pins the warden image and
  `SQUELCH_WARDEN_IMAGE` (the tenant image). `deploy/hosted/60-models.yaml`
  pins the model-warm job's image separately and is easy to forget.

## Preflight, before any tag

There is no CI test gate. A `v*` tag publishes whatever compiles; a
`passband-v*` tag ships whatever notarizes. The tests run on this desk or not
at all:

```sh
DEVELOPER_DIR=/Library/Developer/CommandLineTools cargo test --workspace
(cd passband && ./test.sh)
```

Then:

1. `git status` — this repo routinely has several efforts in flight in one
   working tree. Everything you are about to tag must be committed and pushed;
   everything you are NOT releasing must not be tangled into those commits.
2. Version sync: workspace `Cargo.toml` version matches the tag you are about
   to cut; for the app, `passband/VERSION` matches `project.yml`.
3. Skim `git log <last-tag>..HEAD` for one-way doors (see Rollback below):
   schema changes that rebuild tables, changed env-var names, changed defaults
   that self-hosters inherit.

## Surface 1: server images (`v*` tag)

```sh
git tag v0.X.Y && git push origin v0.X.Y
```

`.github/workflows/release.yml` builds all three images and pushes to GHCR. No
secrets beyond `GITHUB_TOKEN`. **squelchd is amd64 + arm64** (its Dockerfile
cross-compiles, so one runner covers both); **warden and control are amd64
only**, because they build natively and emulating their ONNX build risks the
six-hour job ceiling in a token-holding job. The one box that runs them is
amd64. There is no QEMU step in this workflow, and re-adding one is not how
arm64 comes back — a native arm runner or a cross-compiling Dockerfile is.

Verify (the failure mode is a half-published release):

```sh
gh run watch
for img in squelchd squelch-warden squelch-control; do
  TOKEN=$(curl -s "https://ghcr.io/token?scope=repository:braelyn-ai/$img:pull" | sed -n 's/.*"token":"\([^"]*\)".*/\1/p')
  curl -s -H "Authorization: Bearer $TOKEN" "https://ghcr.io/v2/braelyn-ai/$img/tags/list"
done
```

Publishing images affects nobody by itself. Self-hosters on `:latest` pick it
up on their next `docker compose pull`, so the tag IS the self-host release —
everything in "Self-host compatibility" below applies the moment you push it.

## Surface 2: hosted carrier rollout

The order matters: images first (above), then the box.

```sh
ssh carrier
# 1. Repoint pins, then apply in numbered order (SETUP.md "Apply" section):
#    20-warden.yaml: warden image tag + SQUELCH_WARDEN_IMAGE
#    60-models.yaml: model-warm job tag (chronically forgotten)
kubectl apply -f deploy/hosted/20-warden.yaml
kubectl -n warden rollout status deploy/squelch-warden
curl -sS https://warden.passband.app/healthz   # -> ok
```

Two things about tenant pods that are by design and will surprise you anyway:

- **Existing tenants do not move on their own.** The warden is not a
  controller; it writes objects at provision time only. Changing
  `SQUELCH_WARDEN_IMAGE` affects NEW tenants. Rolling an existing tenant is
  per tenant: `PUT /v1/tenants/<label>/credentials` converges a `failed` or
  `stopped` tenant; an `active` tenant returns 409 and needs
  `DELETE` + re-provision (SETUP.md "Operating notes", upgrade section). This
  also means object-shape changes (new env vars, ports, NetworkPolicy rules,
  Ingress prefixes) reach existing tenants only on re-apply. Budget for it.
- **`imagePullPolicy: IfNotPresent` + a tag the node has seen = no pull.**
  Roll forward with a new tag, not by moving an old one.

If the release touched monitoring (scrape config, dashboards):

```sh
kubectl apply -f deploy/hosted/80-monitoring.yaml       # on carrier
railway up -s grafana                                    # dashboards are baked; UI edits die
```

Litestream: image rollouts do not touch it, but any restore-shaped operation
does. The first line of every restore drill is
`systemctl mask litestream.service litestream-config.timer` — a re-provisioned
tenant creates an empty DB that litestream would happily stream over real
history (PRODUCTION.md, backups section). Read the drill before you need it.

Post-rollout verification, per tenant:

- Pod Ready and the dashboard's "Inside squelchd" row: sync staleness under a
  few minutes, `squelchd_build_info` showing the new version.
- `curl -sS https://<tenant-host>/client/stats` with a bearer -> 200.

## Surface 3: Passband app (`passband-v*` tag)

The canonical path is the tag; CI (`passband-release.yml`) builds, signs,
notarizes, staples, creates the GitHub Release, regenerates the appcast
against ALL prior releases, commits it to `main` (which redeploys the site,
which publishes the feed), and bumps the Homebrew cask. Local fallback:
`passband/release.sh`, same steps from this machine. Details and credentials:
`passband/RELEASING.md`.

Things that bite:

- CI checks the tag against `passband/VERSION` but NOT against
  `project.yml` — keep them mirrored by hand (release.sh checks both, so a
  local `--dry` run is a cheap preflight even when CI does the release).
- The Sparkle EdDSA private key exists in this Mac's login keychain and as the
  `SPARKLE_PRIVATE_KEY` repo secret. Losing both means every installed app
  rejects every future update. It is not in the repo, on purpose.
- The appcast enclosure URLs go through `passband.app/download/*` (302 to
  GitHub Releases), so the site must be up for updates to install; the feed
  itself is baked into the site image at deploy time.
- Verify after: `curl -s https://passband.app/appcast.xml | grep <VERSION>`,
  then Passband → Check for Updates on a machine running the previous build.

## Surface 4: Railway services

| Service | Deploys when |
|---|---|
| `control` (signup.passband.app) | git push to `main` (repo-connected) |
| landing site (passband.app) | git push to `main` (root dir `passband-site/`) |
| relay | `railway up` only |
| prometheus / blackbox / grafana | `railway up -s <name>` only |

The standing lesson (DEPLOY.md §8): every service must have its
config-as-code file path set in service settings; the root `railway.toml`
builds the RELAY, and a service without its own file inherits it and ships
the wrong image. `Dockerfile.broker` has no railway toml yet — create
`railway.broker.toml` before its first deploy.

Note that `control` deploying on every push to `main` means server-side
changes land on signup as soon as they merge, independent of any `v*` tag.
If a control change must move in lockstep with a carrier rollout, push and
roll the carrier promptly.

## Self-host compatibility (checked at every `v*` tag)

- **Schema is forward-only.** `schema.sql` applies on every open
  (`CREATE IF NOT EXISTS` + additive `PRAGMA table_info` migrations, no
  version stamp, no down-migrations). New tables/columns are safe. A
  table-rebuild migration (the `stage2_usage` precedent) or a `NOT NULL`
  column an old writer cannot populate is a ONE-WAY DOOR: call it out in the
  release notes, because downgrade after it means restore-from-backup.
- **Env vars are API.** Renames need the legacy-alias treatment
  (`SQUELCH_ACCOUNT`/`SQUELCH_DB` precedent: honored + deprecation line).
  Defaults baked into `squelchd/Dockerfile` (`SQUELCH_BIND=0.0.0.0:8848`,
  file cred backend, `/data` paths) are load-bearing for every compose file
  in the wild.
- **glibc floor.** squelchd and control images must stay on trixie-or-newer
  bases (ort needs glibc ≥ 2.38). Warden and relay are independent.
- **New workspace crates** must be added to the COPY lists in BOTH
  `squelchd/Dockerfile` and `Dockerfile.control`, or the tag fails in CI.
- Upgrade command in the docs is `docker compose pull && docker compose up -d`
  against `:latest`; bare-metal is `deploy/update.sh`. Whatever those two do
  to a v-1 install IS the self-host upgrade experience.

## Rollback, per surface

- **Images:** repoint the deploy at the previous tag (tags are immutable,
  nothing to rebuild). For hosted tenants this is the same per-tenant
  re-apply cost as the upgrade was.
- **Data:** the schema has no downgrade path. If the bad release wrote
  something an old binary chokes on, rollback is the litestream restore drill
  (per tenant) — a tag repoint alone is not a rollback after a one-way door.
- **Passband:** Sparkle does not downgrade (build number is the commit count
  and only goes up). Rolling back means shipping a NEW `passband-v*` release
  containing the old behavior. Budget an hour, not a minute.
- **Railway:** each service's dashboard can redeploy the previous deployment;
  for repo-connected services, revert the commit on `main`.
- **Control DB** (`/data/control.sqlite3` on Railway) has no backups today.
  A migration that corrupts it has no restore path. Treat control-plane
  schema changes with the same one-way-door respect as tenant schemas.

## Known gaps (fix or at least know)

1. No CI test gate on either tag path. The preflight above is the gate.
2. Nothing ties workspace `Cargo.toml` to the `v*` tag, or `20-warden.yaml`
   pins to tags that exist in GHCR. Both are hand-checked.
3. `60-models.yaml` pins drift behind `20-warden.yaml` (they did already).
4. The warden image on carrier predates the CI job: the running
   `squelch-warden:v0.2.0` in containerd was hand-built and exists in no
   registry. Do not delete it from containerd until the first post-CI tag is
   cut and `20-warden.yaml` is repointed (PRODUCTION.md, registry-gap note).
5. `passband/VERSION` and `project.yml` currently disagree (0.0.2 vs 1.0.0);
   `release.sh` will refuse to run until reconciled, and CI will not notice.
6. There is no fleet-upgrade lever for tenants; every daemon rollout is
   O(tenants) manual re-applies until a reconcile loop exists.
