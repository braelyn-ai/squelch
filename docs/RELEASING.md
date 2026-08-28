# Releasing

The system has five release surfaces that move independently, on different
triggers, with different blast radii. A "release" is usually one of these, not
all of them; know which one you are doing before you start.

| Surface | Trigger | What ships | Who is affected |
|---|---|---|---|
| Server images | push tag `daemon-X.Y.Z` | `ghcr.io/braelyn-ai/{squelchd,squelch-warden,squelch-control}` | nobody, until a deploy points at the tag |
| Hosted carrier | manual ops on `carrier` | warden + tenant pods | every hosted tenant |
| Passband for Mac | push tag `passband-mac-X.Y.Z` | GitHub Release + appcast + Homebrew cask | every Mac user, via Sparkle |
| Passband for iOS | push tag `passband-ios-X.Y.Z` | a TestFlight build | every TestFlight tester |
| Railway services | git push to `main`, or `railway up` | control, landing site, relay, monitoring | signup, passband.app, push relay |

Every tag namespace names its surface, and the three never overlap: pushing a
phone build never rebuilds three Docker images, and cutting a daemon never
asks Apple for anything.

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
  every crate. This is what `squelchd_build_info{version=...}` reports, so bump
  it with the tag or the metric lies about what is running. `release-daemon.yml`
  now enforces it: a `verify` job fails the release, before any registry write,
  unless the tag is exactly `daemon-<workspace version>`.
- **Image tags** — the git tag, verbatim. `daemon-0.0.1` publishes
  `ghcr.io/braelyn-ai/squelchd:daemon-0.0.1` (same for warden and control) and
  moves `latest`. There are no bare numeric image tags any more — no `0.0.1`,
  no `0.0`, no `v0.0.1` — on purpose: GHCR tags are mutable and the carrier
  node pulls `IfNotPresent`, so a recycled number would silently pin a stale
  image, and the prefixed form cannot collide with the retired ones. `latest`
  is published explicitly on every daemon tag, because the self-host docs
  depend on it.
- **Passband** — `passband/VERSION` is the Mac marketing version;
  `project.yml`'s macOS `MARKETING_VERSION` must mirror it, and the iOS target
  carries its own. CI verifies all of it now: `release-passband-mac.yml`
  refuses a tag that is not `passband-mac-<VERSION>` with project.yml agreeing,
  and `release-passband-ios.yml` refuses a tag that is not
  `passband-ios-<project.yml iOS MARKETING_VERSION>`. `release.sh` preflights
  the same things locally. The Sparkle ordering key is the BUILD number
  (`git rev-list --count HEAD`), which is why marketing versions can move
  backwards without stranding installs.
- **Deploy pins** — the tenant image is `SQUELCH_WARDEN_IMAGE` in
  `deploy/hosted/15-warden-config.yaml`, written once and read by both processes
  that render tenants. The **warden's own** image is a pod-spec field, so it is
  written twice, in `deploy/hosted/20-warden.yaml` and
  `deploy/hosted/90-warden-roller.yaml`, and those two must name the same
  `daemon-*` tag: the roller runs this binary, and an older one renders older
  tenants. `deploy/hosted/60-models.yaml` pins the model-warm job's image
  separately and is easy to forget.

**The 2026-08 consolidation.** The tag namespaces used to be `v*` (daemon),
`passband-v*` (Mac) and `ios-v*` (iOS), and the numbers had drifted apart badly
enough that nothing lined up with anything (`passband/VERSION` said 0.0.2 while
project.yml said 1.0.0; a Mac release had already walked 1.0.0 → 0.0.2). All
three were retired, every old tag deleted, and all three versions restarted at
`0.0.1` — Cargo workspace, `passband/VERSION`, and both project.yml targets.
Sparkle orders by build number, so the Mac reset strands nobody. Old numeric
GHCR image tags (`0.2.x`, `v0.2.6`, `v0.1.0`) still exist on the registry as
frozen history: deployments pinned to them keep working, and they are never
written again — repoint to a `daemon-*` tag at the next rollout.

## Preflight, before any tag

No tag path runs the test suite. The only thing CI checks at tag time is that
the version numbers agree; past that, a `daemon-*` tag publishes whatever
compiles and a `passband-mac-*` tag ships whatever notarizes. Tests run on PRs
(`ci.yml`) and on this desk, or not at all:

```sh
DEVELOPER_DIR=/Library/Developer/CommandLineTools cargo test --workspace
(cd passband && ./test.sh)
```

Then:

1. `git status` — this repo routinely has several efforts in flight in one
   working tree. Everything you are about to tag must be committed and pushed;
   everything you are NOT releasing must not be tangled into those commits.
2. Version sync: workspace `Cargo.toml` version matches the tag you are about
   to cut; for the app, `passband/VERSION` matches `project.yml`. CI checks
   both, but it checks them after you have pushed a tag, and a failed tag is a
   tag you have to delete before you can retry it.
3. Skim `git log <last-tag>..HEAD` for one-way doors (see Rollback below):
   schema changes that rebuild tables, changed env-var names, changed defaults
   that self-hosters inherit.

## Surface 1: server images (`daemon-*` tag)

```sh
git tag daemon-0.X.Y && git push origin daemon-0.X.Y   # must equal Cargo.toml
```

`.github/workflows/release-daemon.yml` verifies the tag against the workspace
version, then builds all three images and pushes them to GHCR under the tag
itself plus `latest`. No secrets beyond `GITHUB_TOKEN`. **squelchd is amd64 +
arm64** (its Dockerfile cross-compiles, so one runner covers both); **warden
and control are amd64 only**, because they build natively and emulating their
ONNX build risks the six-hour job ceiling in a token-holding job. The one box
that runs them is amd64. There is no QEMU step in this workflow, and re-adding one is not how
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

**The step-by-step is `deploy/hosted/ROLLOUT.md`** — preflight, the four pins,
the apply order, what converging looks like, and what each failure means. What
follows here is the shape of it; that page is what to have open while you do it.

The order matters: images first (above), then the box.

```sh
ssh carrier
# 1. Repoint pins, then apply in numbered order (SETUP.md §8):
#    15-warden-config.yaml: SQUELCH_WARDEN_IMAGE, the image TENANTS run
#    20-warden.yaml + 90-warden-roller.yaml: the warden's own image, the SAME
#      tag in both (see below)
#    60-models.yaml: model-warm job tag (chronically forgotten)
kubectl apply -f deploy/hosted/15-warden-config.yaml
kubectl apply -f deploy/hosted/20-warden.yaml
kubectl apply -f deploy/hosted/90-warden-roller.yaml
# The ConfigMap is read once, when a pod starts: the roller gets it on its next
# tick, the serving warden only here.
kubectl -n warden rollout restart deploy/squelch-warden
kubectl -n warden rollout status deploy/squelch-warden
curl -sS https://warden.passband.app/healthz   # -> ok
```

**Applying that pin IS the rollout decision, and it is the only one.** The
warden is not a controller: it writes a tenant's objects at provision time and
never revisits them, so a new `SQUELCH_WARDEN_IMAGE` changes what the next
signup gets and nothing about the tenants already running. What closes the gap
is the roller — the CronJob in `90-warden-roller.yaml`, which runs the warden's
own binary as `squelch-warden roll` every 5 minutes under the warden's
ServiceAccount. Each run reads the whole fleet, converges ONE drifted tenant onto
today's render, waits for that rollout to finish, and exits. Each tenant blips
for one pod restart; the fleet is never down; no mail is lost, because Gmail is
the source of truth and the daemon resumes syncing on its next tick.

Five things about it that will surprise you anyway:

- **A bump takes one tick per tenant.** Ten tenants behind is ten runs, two and
  a half hours on the default schedule, and the run says how many are left
  (`still behind, one per run: N more`). The gap between ticks is the safety
  model, not a scheduling accident: a finished rollout only means the API server
  saw a ready replica, and squelchd binds its socket before it finishes starting,
  so what actually clears a render is five minutes of a real daemon serving
  real mail followed by a read pass that refuses to roll anything if that mailbox
  is carrying today's render and not up. **Every tick but the last exits 3 and
  marks its Job failed**, which is by design and is why an alert on failed Jobs
  alone is the wrong alert — see PRODUCTION.md.

- **The serving warden does not see a ConfigMap change until it restarts.**
  `envFrom` is read at pod start. The roller is a fresh pod every 5 minutes and
  picks the new pin up on its own; the warden keeps rendering the old one into
  new signups and into every `llm mint` until
  `kubectl -n warden rollout restart deploy/squelch-warden`. Apply and restart
  together.
- **The roller converges the DEPLOYMENT only.** Drift is computed from that one
  object, so a release that changes a tenant's Service, Ingress, NetworkPolicy
  or PVC lands on new signups and on nobody else — the roll will report those
  tenants as already current. Those releases need `squelch-control reconcile`
  per tenant, which re-applies all five objects. Check the diff before you
  assume the timer has it.
- **A skipped tenant stays skipped.** The roller refuses to touch a Deployment
  another field manager owns fields on (`kubectl set env` is the usual way to
  get one), because the only repair is deleting it — a real outage window for
  that mailbox, and not a timer's decision. That is exit code 2, and it wants
  `squelch-control drift <label>` then `squelch-control reconcile <label>` from
  a person. `pending` tenants and cancelled accounts are skipped too, and still
  want `PUT /v1/tenants/<label>/credentials` (SETUP.md "Operating notes").
- **`imagePullPolicy: IfNotPresent` + a tag the node has seen = no pull.**
  Roll forward with a new tag, not by moving an old one.

Watch it converge, or push it along:

```sh
kubectl -n warden get jobs                            # one row per run
kubectl -n warden logs job/<name>                     # per-tenant lines, then a summary
kubectl -n warden patch cronjob squelch-warden-roll -p '{"spec":{"suspend":true}}'
```

Suspending stops the next tick and not the run in flight, and
`kubectl create job --from=cronjob` makes a standalone Job that
`concurrencyPolicy: Forbid` does not count — two rollers, two mailboxes down at
once. The safe manual-run recipe is in `90-warden-roller.yaml`'s header and in
PRODUCTION.md.

Exit 0 is a converged fleet (nothing to do counts, and so does a clean
`--dry-run`); 1 is a tenant wanting a person while the fleet keeps converging
around it — halted on the tenant it took, a tenant DOWN with no workload and no
cancellation on record, one whose sealed credential is gone, never started, or a
`--dry-run` that found work; 2 is everything it could
converge converged with something left that no run ever will (foreign drift, or
an identity Secret whose label does not validate); 3 is a tenant rolled with more
queued behind it, which is every tick of a normal bump; 4 is a casualty that
froze the fleet, which is the one code worth paging on; 64 is a bad argument
list. Anything but 0 marks the Job failed on purpose, so 3 marks it failed too.
PRODUCTION.md, "Rolling the daemon image", has the full table and what to do
about each, and "Alerting on this, without alerting on normal" has the query.

If the release touched monitoring (scrape config, dashboards):

```sh
kubectl apply -f deploy/hosted/80-monitoring.yaml               # on carrier
kubectl -n monitoring rollout restart deploy/prometheus-agent   # the apply is not the reload
```

The restart is not optional: the agent runs without `--web.enable-lifecycle`
and reads its config once, at boot, so an apply alone leaves it scraping the
config it booted with. Dashboards need no command at all: they are baked into
the Grafana image (UI edits die), and grafana auto-deploys from `main`, so
merging is the deploy.

Litestream: image rollouts do not touch it, but any restore-shaped operation
does. The first line of every restore drill is
`systemctl mask litestream.service litestream-config.timer` — a re-provisioned
tenant creates an empty DB that litestream would happily stream over real
history (PRODUCTION.md, backups section). Read the drill before you need it.

Post-rollout verification. The first stop is the roll Job's log, which names
every tenant it moved and every one it could not; then, per tenant:

- Pod Ready and the dashboard's "Inside squelchd" row healthy: sync staleness
  under a few minutes, errors flat. The dashboard has no version panel, so
  confirm what is actually running from the pod itself —
  `kubectl -n tenants get pod <label>-... -o jsonpath='{..image}'`, or scrape
  `squelchd_build_info` off port 9464 from the monitoring namespace.
- `curl -sS https://<tenant-host>/client/stats` with a bearer -> 200.
- `squelch-control drift <label>` comes back with nothing in either list. A
  Ready pod only proves something started; this proves it is the thing this
  release renders.

**The first time `drift` is ever run against this cluster, treat its output as
unproven.** Its whole test suite runs against a mock with no server-side-apply
merge in it, so five things are only assertions until a real API server has
answered them. Check them once, on one tenant, and then trust the command:

1. **A freshly provisioned tenant reports zero changes.** Both sides of the
   diff come back from the API server, so defaulting should cancel out — but
   `Quantity` canonicalization (`1Gi` vs `1073741824`), `creationTimestamp:
   null` on the pod template, and defaulted `protocol` / `dnsPolicy` /
   `schedulerName` are the candidates for a permanent false positive. If one
   shows up on every tenant, that field wants filtering, not reconciling.
2. **A hand edit lands in `foreign` and NOT in `changes`.** `kubectl set env`
   something harmless onto a scratch tenant. The field survives the dry-run
   merge on both sides, so the ledger is the only half that should see it. If
   it shows up in `changes` instead, the dry run is not merging and the whole
   two-detector split needs rethinking.
3. **An overwrite of a field the warden DOES declare lands in `changes`.**
   `kubectl set image` on the same scratch tenant: expect `live` = the hand-set
   tag, `rendered` = `SQUELCH_WARDEN_IMAGE`, and no `409` out of the dry run.
4. **A recreate really does empty the ledger.** After a `reconcile` that
   answers `recreated`, `kubectl -n tenants get deploy <label> --show-managed-
   fields -o yaml` should carry exactly one manager entry, `squelch-warden`.
   That is the claim the entire route rests on.
5. **The cancellation marker lands, and it lands as a MERGE PATCH.** `DELETE`
   the scratch tenant, then read its identity Secret back:

   ```sh
   kubectl -n tenants get secret <label>-identity -o yaml
   ```

   The annotation `passband.email/cancelled-at` must be there, **and every key
   under `data:` must still be there with it** — that Secret holds the age key
   every credential the tenant ever had was sealed to, and a patch that arrived
   as a server-side apply would have taken it. This is the one write in the
   service that is not an apply, and this is the check that it stayed that way.
   Then `PUT` credentials back and confirm the annotation is gone: reopening is
   the only thing that clears it.

Then delete the scratch tenant. Doing this on a tenant with real mail in it is
how a verification becomes an incident.

## Surface 3: Passband for Mac (`passband-mac-*` tag)

The canonical path is the tag; CI (`release-passband-mac.yml`) builds, signs,
notarizes, staples, creates the GitHub Release, regenerates the appcast
against ALL prior releases, commits it to `main` (which redeploys the site,
which publishes the feed), and bumps the Homebrew cask. Local fallback:
`passband/release.sh`, same steps from this machine. Details and credentials:
`passband/RELEASING.md`.

Things that bite:

- CI checks the tag against BOTH `passband/VERSION` and `project.yml`'s macOS
  target before it builds anything, so drift is now a failed release rather
  than a wrong number in the wild. `release.sh --dry` checks the same pair
  locally, which is the cheaper place to find out.
- The Sparkle EdDSA private key exists in this Mac's login keychain and as the
  `SPARKLE_PRIVATE_KEY` repo secret. Losing both means every installed app
  rejects every future update. It is not in the repo, on purpose.
- The appcast enclosure URLs go through `passband.app/download/*` (302 to
  GitHub Releases), so the site must be up for updates to install; the feed
  itself is baked into the site image at deploy time.
- Verify after: `curl -s https://passband.app/appcast.xml | grep <VERSION>`,
  then Passband → Check for Updates on a machine running the previous build.

## Surface 4: Passband for iOS (`passband-ios-*` tag)

`release-passband-ios.yml` verifies the tag against `project.yml`'s iOS
`MARKETING_VERSION`, archives, exports, and uploads to TestFlight. Local
equivalent: `passband/release-ios.sh`. Two things differ from the Mac path:

- **No signing material anywhere.** Three App Store Connect secrets
  (`ASC_KEY_ID`, `ASC_KEY_ISSUER_ID`, `ASC_KEY_P8`) go to xcodebuild and cloud
  signing mints a managed distribution certificate per run. Nothing to lose,
  nothing to rotate on a schedule.
- **The build number is the UTC minute**, not the commit count, and App Store
  Connect refuses a marketing version that ever goes backwards. The verify job
  exists so the tag cannot free-float away from what a local build would stamp.

## Surface 5: Railway services

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
changes land on signup as soon as they merge, independent of any `daemon-*`
tag. If a control change must move in lockstep with a carrier rollout, push and
roll the carrier promptly.

## Self-host compatibility (checked at every `daemon-*` tag)

- **Schema is forward-only.** `schema.sql` applies on every open
  (`CREATE IF NOT EXISTS` + additive `PRAGMA table_info` migrations, no
  version stamp, no down-migrations). New tables/columns are safe. A
  table-rebuild migration (the `stage2_usage` precedent) or a `NOT NULL`
  column an old writer cannot populate is a ONE-WAY DOOR: call it out in the
  release notes, because downgrade after it means restore-from-backup.
- **Sanitized html is API too, and it is written before it is read.**
  `body_html` is cleaned once at ingest (`sync/html.rs`) and never rewritten, so
  a change there reaches readers running last month's app and keeps reaching
  them for every message synced in between. The `cid:` admission is the
  precedent: the scheme survives sanitization only because the client pairs the
  reference with an attachment row and rewrites it, and any build older than the
  one carrying `Lib/CidImages.swift` paints a broken box over every inline photo
  instead. Ship the app release first, or the daemon tag alone is a visible
  regression on every reader that has not updated.
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

- **Images:** repoint the deploy at the previous `daemon-*` tag and re-apply;
  nothing to rebuild. GHCR would happily let you move a tag instead — never do
  it, because a tag that is written once is the only reason a repoint IS a
  rollback under `imagePullPolicy: IfNotPresent`. Deployments still pinned to a
  pre-consolidation numeric tag can roll back to one of those; the old images
  were not deleted from the registry. For hosted tenants this is the same
  per-tenant re-apply cost as the upgrade was.
- **Data:** the schema has no downgrade path. If the bad release wrote
  something an old binary chokes on, rollback is the litestream restore drill
  (per tenant) — a tag repoint alone is not a rollback after a one-way door.
- **Passband:** Sparkle does not downgrade (build number is the commit count
  and only goes up). Rolling back means shipping a NEW `passband-mac-*` release
  containing the old behavior. Budget an hour, not a minute. iOS is the same
  shape with Apple in the loop: a new `passband-ios-*` build, or expire the bad
  one in TestFlight.
- **Railway:** each service's dashboard can redeploy the previous deployment;
  for repo-connected services, revert the commit on `main`.
- **Control DB** is the project's Railway Postgres service. Railway keeps its
  own backups of managed Postgres, which is more restore path than the old
  volume file ever had — but a migration that corrupts data still deserves
  the same one-way-door respect as tenant schemas: verify against a copy
  before shipping schema changes.

## Known gaps (fix or at least know)

1. No CI test gate on any tag path — the tag workflows verify versions, not
   behavior. The preflight above is the gate.
2. Nothing checks that the deploy pins name tags that exist in GHCR — neither
   `SQUELCH_WARDEN_IMAGE` in `15-warden-config.yaml` nor the warden image in
   `20-warden.yaml` and `90-warden-roller.yaml`; those are still hand-checked,
   and so is the fact that the last two agree with each other. (The
   tag-to-`Cargo.toml` half is closed — `release-daemon.yml` verifies it.)
3. `60-models.yaml` pins drift behind the warden's (they did already).
4. The warden image on carrier predates the CI job: the running
   `squelch-warden:v0.2.0` in containerd was hand-built and exists in no
   registry — that tag is from the retired numbering and nothing will ever
   publish it. Do not delete it from containerd until a `daemon-*` tag is cut
   and `20-warden.yaml` is repointed at a tag the registry actually holds
   (PRODUCTION.md, registry-gap note).
5. Nothing alerts when a roll does not converge. The roller walks the fleet
   every 5 minutes and halts on the first tenant that does not come back, which
   is the right behavior and is invisible: it shows up as a failed Job in ns
   `warden` and nowhere else, with 72 runs of history — six hours at this
   schedule — before the evidence rotates away. kube-state-metrics is already
   scraped off carrier, so the alert is
   `kube_job_status_failed{namespace="warden"} > 0`; it wants writing.
