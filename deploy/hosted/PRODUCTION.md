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
| Node memory | 3814 MiB total, and **all of it is offered to pods today**: nothing is reserved, so `kubepods.slice` `memory.max` is the whole 3.72 GiB, while the host's own processes (k3s, its containerd, journald, litestream, sshd) hold ~1.2 GB that is outside every pod's budget. `SETUP.md` §2b reserves 1200Mi and evicts at 200Mi, which leaves ~2414Mi allocatable and room for five tenants. **Not applied here yet** |
| Swap | none. §2b's 2 GB zram device is written (`deploy/hosted/node/`) and **not installed here yet**. With it, `LimitedSwap` rations about 206 MB per tenant at a 384Mi request |

Shared vCPU on purpose: tenant daemons are idle between syncs, and dedicated
(CCX) costs roughly triple for headroom this workload spends most of its life not
using. Scale vertically by resizing **CPU and RAM only** — a Hetzner resize that
grows the disk is a one-way door and blocks every later downsize.

### If a tenant is OOM-killed, read the constraint first

`kubectl` will tell you a container restarted. Only the kernel says which of the
two kinds of out-of-memory that was, and they have different answers.

```sh
ssh carrier "journalctl -k --since '-24h'" | grep -i 'out of memory'
```

`constraint=CONSTRAINT_MEMCG` with an `oom_memcg=` naming one tenant's cgroup is
that container over its own limit: the culprit is the victim, and the answer is
its limit or its code. `constraint=CONSTRAINT_NONE` with `global_oom` is the box
running out with nobody over their limit, and the kernel then picks by badness
across everything on it, which means largest RSS, which means a tenant daemon
unrelated to whatever actually grew. That is the shape of the 2026-08-19 kills;
`SETUP.md` §2b is the arithmetic and the fix, and none of §2b is applied here
yet.

There is a third case that can only appear after §2b(a) is applied:
`oom_memcg=/kubepods.slice`, the pods collectively over the node's allocatable.
It picks its victim the same way, by largest RSS in the subtree, so it is no
kinder to the tenant that dies; what it changes is that the host processes are
no longer eligible and nothing outside the cluster is at risk. It also arrives
with no eviction event and no node condition, so
`/sys/fs/cgroup/kubepods.slice/memory.events` is where you count them.

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

## How many tenants fit

**Five, and memory is what decides it.** Not CPU, not the `pods: "25"` line in
`70-tenant-limits.yaml`, and there is no swap under any of it.

A tenant daemon that has embedded anything keeps its ONNX session resident. The
four tenant pods here read 91, 317, 376 and 509 MiB on 2026-08-26, and 123, 293,
349 and 545 MB on another pass the same day: roughly 300-500 MB at rest, moving
while you watch. The warden requests **384Mi** per tenant
(`15-warden-config.yaml`), which is about the p50 of that rather than a
comfortable number, and `70-tenant-limits.yaml` holds the namespace to
`requests.memory: 1920Mi`, which is five of them. The subtraction is written out
in that file and in `SETUP.md` §7: 3814 MiB of machine, less about 1200 for k3s,
containerd, journald and litestream, less about 372 for the pods that are not
tenants, leaving about 2.2 GiB.

**The quota is the only refusal on this box.** Nothing is reserved outside the
pod budget today: k3s takes nothing for itself by default, so the scheduler
still sees all 3814 MiB as allocatable and would admit a sixth tenant and a
seventh onto a machine that cannot run them. A kubelet `systemReserved` is what
would make the scheduler a second gate; it is written up as `SETUP.md` §2b (PR
#151) and **not applied here**.

What that combination costs when the numbers are wrong is 2026-08-19: the
request was 256Mi and the quota was 5Gi on a 4 GB box, four tenants ran it out
of memory globally, and the kernel OOM-killed two squelchd processes. It takes a
running mailbox, because a tenant pod is burstable, while the signup that
overcommitted the box succeeded days earlier and is nobody's suspect. With both
numbers telling the truth the sixth signup is refused instead, as the documented
`500 not_ready`, reason in `kubectl -n tenants describe replicaset`.

A sixth tenant is a RAM resize (CPU and RAM only, see "The box") or the
daemon-side embedder unload landing and being measured for a week, whichever
comes sooner. Raise it off `container_memory_working_set_bytes`, not off wanting
a sixth tenant. Existing tenants take a changed bound the way they take any
pod-shape change: see "Shipping a tenant-shape change".

## Railway

| | |
|---|---|
| Service | `control` — `squelch-control`, the signup plane |
| Build | `railway.control.toml`, set as the service's **Config-as-code file path** |
| Domain | `signup.passband.app` (CNAME to the Railway target) |
| Store | the project's Railway **Postgres** service, referenced as `SQUELCH_CONTROL_DATABASE_URL = ${{Postgres.DATABASE_URL}}` (private URL). The `/data` volume only survives until the one-time `import-sqlite` cutover, then it is retired |
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

### The Postgres cutover (one-time, then delete this section)

The store moved from the volume's SQLite file to the project's Postgres
service. The order matters:

1. Add the Railway Postgres service; on `control` set
   `SQUELCH_CONTROL_DATABASE_URL = ${{Postgres.DATABASE_URL}}` — the
   **private** URL (the store speaks no TLS; `DATABASE_PUBLIC_URL` is the
   TCP proxy plus egress fees). Private DNS is IPv6-only and can lag a fresh
   boot by a beat; the `on_failure` restart policy absorbs a failed first
   connect.
2. Deploy. The service is live on an empty store from here until step 3 —
   minutes, and a signup that lands in the window makes the importer refuse
   on a non-empty table, which is the designed failure: reconcile by hand,
   re-run.
3. `railway ssh` into `control` (lands as root, env present):
   `squelch-control import-sqlite /data/control.sqlite3`. It copies all
   three tables with ids preserved, re-arms the sequences, and prints counts
   only.
4. Verify: `squelch-control tenants`, `squelch-control invite list`, and the
   `/admin` board against known counts.
5. Copy `control.sqlite3` off the box **before** touching the volume —
   deleting a Railway volume destroys it. After a soak (one invite issued,
   one signup completed on Postgres), remove the volume; a cleanup PR then
   drops the entrypoint chown, `rusqlite`, and `import.rs`.

Rollback until step 5: redeploy the previous image — the importer opened the
file read-only, so it is exactly as it was, minus rows that landed in
Postgres after cutover.

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
`15-warden-config.yaml`: `SQUELCH_WARDEN_LLM_BASE_URL` (the feature gate — the warden
refuses to boot if any of the rest are set without it),
`SQUELCH_WARDEN_LLM_STAGE1_MODEL`, `SQUELCH_WARDEN_LLM_STAGE2_MODEL`,
`SQUELCH_WARDEN_LLM_STAGE1_DAILY_CAP`, `SQUELCH_WARDEN_LLM_STAGE2_DAILY_CAP`.
Install procedure: `SETUP.md` → "LLM triage through the gateway".

**A model is allow-listed twice, in two different spellings.** This is the
single most load-bearing fact about the gateway and it has now caused two
separate multi-day outages.

| Where | Spelling it matches | Written by |
|---|---|---|
| Virtual key `allowed_models` | the id the daemon sent, `anthropic/claude-opus-5` | `squelch-control llm mint` |
| Provider key `models` | the same id with the provider resolved away, `claude-opus-5` | `squelch-control llm sync` |

A model present in the first and absent from the second answers **400 `no keys
found that support model: <model>`**. Nothing about that failure is visible
from the cluster: the daemon's config is right, the warden's config is right,
the tenant's virtual key is right, and the call still dies at the gateway. Run
`squelch-control llm sync` after every change to the fleet's model, and
`squelch-control llm sync --check` to ask whether everything agrees now.

> **History, 2026-08-21 to 2026-08-25: the fleet was dark and the canary that
> exists to say so was also dark.** Two independent faults, and either one
> alone would have been caught in an hour.
>
> The fleet fault: the provider key's `models` still read `claude-opus-4-8`
> from the gateway's first setup, months after everything else moved to
> `claude-opus-5`. Every tenant's Stage-1 and Stage-2 call 400'd at routing and
> every row fell back to its ingest heuristic. Bifrost's own budget ledger read
> `current_usage: 0` for every tenant key, which is what "the whole fleet
> triaged nothing" looks like from the outside. Fixed by putting that list
> under `squelch-control llm sync`; it had no owner before, which is why it
> outlived two model migrations.
>
> The canary fault: the canary's virtual key had been typed into the Bifrost UI
> comma-space separated, so its `allowed_models` were stored as
> `["anthropic/claude-opus-5 ", "claude-opus-5 "]`. The gateway matches
> exactly. Every probe answered 403 while the list rendered as correct in the
> UI and in every JSON listing, because a trailing space inside a quoted string
> is invisible in both.
>
> And the reason four days passed: the Grafana panel queried
> `probe_success{job="blackbox-bifrost-llm"}` as an instant query against a job
> that scrapes every 15m. An instant query looks back 5m, so the series was
> absent two thirds of the time and the panel rendered its `noValue` string —
> `NO DATA`, which the panel's own description glosses as "the canary itself is
> not scraping". The one panel that exists to say the fleet cannot reach its
> model spent most of its life pointing at the monitoring instead, and was
> never once seen red. Now wrapped in `last_over_time[20m]`.
>
> The lesson worth keeping is not any of the three bugs. It is that a probe
> whose failure mode renders as "no data" is not a probe. Check what a red
> canary actually looks like on the dashboard, not just that a green one does.

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

The two tags live in different places, and the split is deliberate. The squelchd
image tenants run is `SQUELCH_WARDEN_IMAGE` in `15-warden-config.yaml`, written
once and read by both processes that render tenants — the serving pod and the
roller — through `envFrom`. The **warden's own** image is a pod-spec field, which
no ConfigMap can carry, so it is written twice: in `20-warden.yaml` and in
`90-warden-roller.yaml`. Those two must name the same tag, because this binary is
the renderer and a roller on an older one renders older tenants. It is the one
value in this system that two files still have to agree on by hand:

```sh
kubectl -n warden get deploy/squelch-warden \
  -o jsonpath='{.spec.template.spec.containers[0].image}{"\n"}'
kubectl -n warden get cronjob/squelch-warden-roll \
  -o jsonpath='{.spec.jobTemplate.spec.template.spec.containers[0].image}{"\n"}'
```

The warden refuses to start with an untagged tenant image.

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

**The roller converges the DEPLOYMENT, and only the Deployment.** This is the
limit to know before the roll below is trusted with a shape change. What decides
whether the roller touches a tenant is a drift report, and a drift report renders
and diffs that tenant's Deployment alone — its Service, Ingress, NetworkPolicy
and PVC are never rendered and never compared. Delete a tenant's Ingress out of
band and the roll reports that tenant as **already current** while the mailbox is
unreachable from the internet; that is verified behaviour, not a worry.

So a change to `SQUELCH_WARDEN_NODE_CIDR` (a NetworkPolicy rule),
`SQUELCH_WARDEN_TLS_SECRET` or the ingress class (an Ingress field),
`SQUELCH_WARDEN_STORAGE_*` (the PVC), or a new path in `HUMAN_DOOR_PREFIXES`
reaches **new signups only, forever**. No roll will ever report an existing
tenant as drifted for one, and no roll will ever deliver one. The answer is
manual and per tenant:

```sh
squelch-control reconcile <label>     # re-applies PVC, NetworkPolicy, Service, Deployment, Ingress
```

Walk the list from `squelch-control tenants`, one label at a time, watching each
pod come back. A tenant that happens to be rolled for some Deployment-visible
reason picks the other four objects up as a side effect, because `reconcile`
re-applies all five — that is luck, and it is not a plan.

**One tenant at a time, verified.** That is the whole discipline, and the roller
below is it done by a timer that does not get bored on tenant nine — except the
timer goes further than a person would: it converges exactly ONE tenant per run
and then leaves, so the gap between ticks is a real daemon serving real mail
before the next mailbox is touched. Driving it by hand is still the right move
when you want to choose the ORDER — reconcile the least important label, watch
the pod come back, run `drift` on it again — and fleet `drift` tells you how
long the list is without touching anything.

**A reconcile can be stopped by its own caller, and that is the usual way one
dies partway.** The warden's reconcile runs inside the request handler, so the
work is tied to the connection: when the client gives up, reqwest drops the
connection, axum drops the handler future, and the warden stops mid-operation
with no terminal log line, because the code that would write one never runs.
Which window it stops in is a coin flip on how fast the old pod releases its
`ReadWriteOnce` volume. Land after the apply and kube brings the pod up on its
own and nobody notices; land before it and the mailbox is down until somebody
finishes the job. On 2026-08-19 two tenants were reconciled minutes apart, both
printed the same error, and only one of them was still serving afterwards.

Two consequences to hold on to:

1. **The warden's log is the record of what happened, not the CLI's answer.**
   The line that settles it is

   ```sh
   kubectl -n warden logs deploy/squelch-warden | grep -E 'tenant=<label>($| )'
   ```

   and `kubectl -n tenants get deploy <label>` says which side of the apply it
   stopped on. A `NotFound` there is a mailbox that is down right now.

   **Both obvious spellings of that grep are wrong, and one of them cost us the
   incident.** The warden logs through `tracing_subscriber::fmt()`'s default
   format, which writes the message first and the fields after it, so
   `tenant=<label>` is usually the LAST token on the line and a
   `grep "tenant=<label> "` matches nothing. That is not theoretical: the line
   that reported the mailbox down on 2026-08-19 was

   ```
   WARN squelch_warden::provision: a tenant with no workload and no cancellation
   on record; a job that did not finish left it down tenant=ellie
   ```

   and a trailing-space match would have skipped straight past it. Dropping the
   space instead over-matches, because labels share prefixes: `tenant=ellie`
   also finds `ellie-atuin`, and prefix matching already sent somebody to the
   wrong tenant once during that incident. End of line or a following space are
   the two ways a label actually ends, so match both and neither more. The same
   regex is baked into the CLI's own timeout message.
2. **The two failures say different things and are not interchangeable.**
   `did not answer in time and may still be working` means the call landed and
   the warden may be working on it this second — do not retry blind, go read the
   log. `could not be reached` means the call never landed and nothing was
   started. Until 2026-08-19 both printed the second message, which is why that
   incident was read as "nothing happened" and left alone for eight minutes.

`reconcile` gets its own ten-minute client budget for this reason
(`RECONCILE_TIMEOUT` in `squelch-control/src/config.rs`); everything else,
`drift` included, keeps 30 seconds. Ten minutes covers `2 * ready_timeout` plus
the applies for any `SQUELCH_WARDEN_READY_TIMEOUT_SECS` up to about 300, and
nothing checks that pairing for you — `control` cannot see the warden's
configuration. **Raise that constant if you ever raise the ready timeout above
300.** To run one on your own budget instead of the CLI's:

```sh
TOKEN=$(kubectl -n warden get secret squelch-warden -o jsonpath='{.data.token}' | base64 -d)
WARDEN_URL=https://warden.passband.app
curl -sS -X POST -m 600 -H "Authorization: Bearer $TOKEN" \
  "$WARDEN_URL/v1/tenants/<label>/reconcile"
```

Detaching the work from the connection — a `202` and a status route to poll, or a
handler that is not dropped when the caller goes away — is the real fix and is
tracked as issue #91. Until it lands, every reconcile is a call somebody has to
stay on the line for.

**A reconcile that died in its own delete/apply window** leaves the tenant
reading `stopped`, because for that moment it has no Deployment. Wait for the
old pod to finish terminating and run the same `reconcile` again: it resumes
rather than refusing, because nothing recorded a cancellation here and that is
what the warden asks. Nothing was lost — the volume, the identity and the sealed
credential never moved.

How it knows is a marker and not a shape. `DELETE` stamps
`passband.email/cancelled-at` on the tenant's identity Secret — the one object a
cancellation deliberately keeps — **before** it removes anything, so every
prefix of a teardown that failed partway still says "this account is closed",
including the prefix where the Deployment is still up and serving. Read it with:

```sh
kubectl -n tenants get secret <label>-identity \
  -o jsonpath='{.metadata.annotations.passband\.email/cancelled-at}{"\n"}'
```

An empty answer means nobody cancelled this tenant and a `reconcile` will finish
the job. `set_credentials` — reopening the account — is the only thing that
clears it, and it clears it LAST, after the workload it promised is up: a reopen
that dies partway leaves the account closed and retryable rather than leaving a
mailbox serving on a cancelled credential with nothing on record saying so.

**Tenants cancelled before this warden shipped carry no marker at all**, and
they are the one case where the answer above is not the whole story. See
"Tenants cancelled before the marker existed" below before you act on an empty
answer for an account you believe was closed.

**A tenant that is marked cancelled AND still serving means its teardown failed
partway.** `DELETE` removes four objects one at a time and stops at its first
error, so a failure on the Ingress leaves the mailbox up, on a credential its
owner has already cancelled. `reconcile` and the roller both refuse it, and
correctly — it is a closed account, not a shape to repair. **Run `DELETE`
again**: the marker is already there, the call is idempotent, and it finishes
the teardown from wherever it stopped. (Reopening it also works and is not a
workaround: `PUT .../credentials` is exempt from the usual "already provisioned"
409 for exactly this tenant, so the account holder is never locked out by a
teardown that half-failed.)

**One narrow race survives that**, and it is worth knowing before you cancel an
account while a roll is in flight: a reconcile reads the tenant's Deployment and
then applies it, and a `DELETE` landing between those two calls is not seen by
either. The marker closes every wider version of this (a `DELETE` before the
read is refused outright, and one that removes the Deployment mid-run is caught
when the apply path finds it missing), and the pacing shrinks the exposure to
one tenant per tick rather than every active tenant per tick. It does not close
that last window. Cancel deliberately rather than casually — suspend the
CronJob, or confirm the tenant is still gone once the run finishes
(`kubectl -n tenants get deploy,svc -l app.kubernetes.io/instance=<label>`).
`squelch-warden/README.md`, "What the drift report cannot see", has the
mechanism.

**The fallback, for what reconcile refuses.** A `pending` tenant comes back as
`409 not_reconcilable`, and one whose identity Secret carries the cancellation
marker — whatever its status word and whatever objects are still standing — as
`409 cancelled`. Same status, two words, because the next move is different:
finish the signup, or re-consent. `POST .../pair` and `PUT .../llm-key` refuse a
cancelled account the same way, for the same reason — a pairing code is full
access to a mailbox, and an LLM key is a live gateway credential the teardown
deliberately deleted.

Reopening one is **re-consent, not a re-`PUT`**. Nothing outside the tenant's
own Secret holds a copy of that ciphertext: `squelch-control`'s schema carries
no tokens and no ciphertext by design, and the refresh token it seals exists in
memory for the length of one signup request. So the person signs in again, the
control plane seals a fresh credential to the recipient the warden still holds,
and `PUT /v1/tenants/{label}/credentials` rebuilds every object from today's
code. The volume, the identity and the old sealed blob were never touched; the
mailbox is down for the length of a provision.

### Tenants cancelled before the marker existed

**Read this once, at the deploy that introduces the marker, and then never
again.**

The warden this one replaces recorded a cancellation nowhere. It deleted the
Ingress, the Service, the Deployment and the NetworkPolicy and left the identity
Secret, the credential Secret and the volume — which is byte-for-byte what a
`reconcile` that died in its own delete/apply window leaves too. The marker is
what tells those apart from now on, and a tenant cancelled before the marker
existed has none.

Nothing about that is silent, and nothing about it needs a data migration: the
warden falls back to the signal the old one used, which is the **Service**.
Every path that puts a workload up applies the Service first, and every teardown
takes the Service before the Deployment, so:

- **Service standing over a missing Deployment** → an interrupted reconcile.
  `reconcile <label>` finishes it, exactly as before.
- **Neither** → a teardown. `reconcile` answers `409 cancelled` and the roller
  files the tenant under "no workload to converge" rather than naming it DOWN.

That fallback is why `10-warden-rbac.yaml` still grants `get` on services, and
why it must be applied **before** the new image rather than after. It is the one
place left in the warden that reads intent out of shape, and it can be deleted —
along with the RBAC verb — once no tenant cancelled by the old warden remains.

**The one thing the fallback cannot see** is a teardown that failed partway
*before* this deploy: a `DELETE` that died on the Ingress left the Deployment up
and running, so the tenant reads `active`, drifts like any other, and this
warden will roll it like any other. Going forward the marker covers that case;
retroactively there is nothing to read. Before you apply the new image, check
the control plane for accounts it believes are cancelled and confirm each one
has no workload:

```sh
kubectl -n tenants get deploy -l app.kubernetes.io/instance=<label>
```

Anything still standing wants a `DELETE` (which now writes the marker first, and
is idempotent) before the roller gets to it.

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

> **Doing one right now?** `ROLLOUT.md` is the checklist, in order, with the
> commands. This section is why it is shaped that way.

Three steps, and only the middle one is a decision.

1. **CI publishes.** A `daemon-X.Y.Z` tag builds
   `ghcr.io/braelyn-ai/squelchd:daemon-X.Y.Z` and pushes it to GHCR
   (`docs/RELEASING.md`, surface 1). Nothing on this box moves, and no tenant is
   affected: the registry holding an image is not a deploy.
2. **A human bumps the pin and applies.** `SQUELCH_WARDEN_IMAGE` in
   `15-warden-config.yaml`, which is one entry in one ConfigMap that both
   rendering processes read. This is the deliberate decision to roll the fleet,
   and it is the only step with a person in it:

   ```sh
   kubectl apply -f deploy/hosted/15-warden-config.yaml
   kubectl -n warden rollout restart deploy/squelch-warden
   ```

   The restart is not optional and it is not cosmetic. `envFrom` is read once,
   when a pod starts: the roller gets the new value on its next tick because
   every run is a fresh pod, and the serving pod goes on rendering the OLD image
   into new signups and into every `llm mint` until it is restarted. Both
   commands, same minute — five minutes is the whole window in which the two
   disagree.
3. **The roller converges the fleet.** The CronJob in `90-warden-roller.yaml`
   runs `squelch-warden roll` every 5 minutes, on the warden's image, under the
   warden's own ServiceAccount, with the warden's own environment — the same
   ConfigMap, through the same `envFrom`. It reads every tenant in the cluster,
   converges ONE whose live Deployment no longer matches today's render, waits
   for that rollout to actually finish, and exits. **A fleet with N tenants
   behind needs N ticks** — ten tenants is two and a half hours — and the run
   says how many are left (`still behind, one per run: N more`).

A **warden** release, as opposed to a daemon one, is the other half: the
`image:` on the Deployment in `20-warden.yaml` and on the CronJob in
`90-warden-roller.yaml`, which are two pod-spec fields no ConfigMap can hold.
Bump both, apply both, and check them against each other (the two `jsonpath`
lines under "Images"). A roller on an older warden image is an older renderer,
and it rolls the fleet onto whatever that older code renders.

Nothing in that chain reaches in from outside. It is the warden's binary calling
its own library code on the box: no credential leaves the cluster to trigger a
roll, GitHub Actions has no power over production, and the control plane's
bearer token buys no part of it.

**What it costs a tenant: one pod restart.** Zero downtime per tenant is not on
offer and never will be — the strategy is `Recreate`, the volume is
`ReadWriteOnce`, and the store is one SQLite file, so two daemons on one mailbox
would corrupt it. The tenant's console and API are unreachable for the length of
one pod cycle.

**What it costs the fleet: nothing, and a bad render costs exactly one tenant.**
One tenant is down at a time, and only while its replacement comes up. That
guarantee comes from the SCHEDULE and not from a health check inside the run: a
finished rollout only means the API server saw a ready replica, and by default a
tenant's probe is a TCP accept on a socket squelchd binds before it finishes
starting. So the run converges one mailbox and leaves, five minutes of real
traffic happen, and the next tick's read pass refuses to roll anything at all if
that mailbox is carrying today's render and not serving it.

**No mail is lost, and that is not a hedge.** Gmail holds the mail; the tenant's
store is a local index of it. A daemon that is restarted mid-sync resumes on its
next tick and re-fetches whatever it had not finished, and the volume, the
identity Secret and the sealed credential are not touched by a roll at all.

The run's answer is its exit code, which is also the Job's status:

| Exit | Means | What to do |
|---|---|---|
| 0 | The fleet is on today's render and serving it. A run with nothing to do is this, and so is `--dry-run` over a fleet that needs nothing. | Nothing. |
| 1 | A tenant wants a person, and the fleet is still converging around it. The summary line says which. | At most the one named tenant was written. Then, by case — see the paragraphs below. Several of these are permanent until you act, so this code can repeat every tick indefinitely. |
| 2 | Everything this run could converge did, and something is left that no run will ever fix: a tenant skipped for foreign drift, or an identity Secret whose label does not validate. | For foreign drift: `squelch-control drift <label>`, then `reconcile <label>` when you are ready for that mailbox to be down for a pod cycle. For an unreadable label: see below. Nothing fixes itself. |
| 3 | The fleet is behind and the run is working through it. Usually "it rolled a tenant and more are queued"; also the tick that spent its one attempt on a tenant that turned out to be foreign or cancelled mid-flight. **Normal.** | Nothing. The next tick takes the next one. If N stops falling across runs, read the stall note in `90-warden-roller.yaml`. A foreign skip raised during a run like this surfaces as a 2 once the queue drains. |
| 4 | **FROZEN.** A casualty stopped the run before it wrote anything: a tenant carries today's render and is not serving it. No tenant converged, and none will on any tick until this is dealt with. | Suspend the CronJob and look at that pod. This is the roller saying the release is bad. **This is the code to page on.** |
| 64 | The Job's argument list is wrong. | Fix `args:` in `90-warden-roller.yaml`. Nothing was read and nothing was applied. |

The six shapes of a 1, and what each wants:

- **Halted on a tenant** (`HALTED on <label>`) — that tenant's reconcile did not
  finish. `kubectl -n tenants logs deploy/<label>` and
  `kubectl -n tenants describe pod -l app.kubernetes.io/instance=<label>`. It is
  the only tenant this run wrote to, and it goes back on the queue for the next
  tick; everything else in the fleet is exactly as the last run left it.
- **Casualty** (`HALTED before applying anything`) — no longer a 1. It is exit
  **4**, because it is the only outcome that stops the rest of the fleet; below.
- **A tenant DOWN with no workload** — below.
- **A tenant that can never be rendered** (`a workload whose sealed credential
  Secret is gone`) — its `<label>-credential` Secret is missing, so the warden
  cannot build the render to compare or apply. The run deliberately does NOT
  halt on it: that state never resolves on its own, and stopping there would
  park every tenant after it in alphabetical order behind a run that fails at
  the same label every five minutes. The pod is probably still serving (the
  daemon copied its credential onto its own volume long ago), so this is not
  urgent in the way a DOWN tenant is — but nothing will ever roll that mailbox
  again until a person puts the Secret back, and there is no automated way to:
  the ciphertext existed in one place. Reopening the account
  (`PUT .../credentials`) is the recovery.
- **Never started** — a config value the warden refused, or an API server it
  could not reach. There are no per-tenant lines at all in the log, only the
  sentence. Nothing was applied.
- **`--dry-run` found work** — the flag doing its job: the fleet is behind and
  this run said so without touching anything. Read the `would roll` list, whose
  length is how many ticks the real roll will take.

**An unreadable label** (exit 2, `identity Secrets whose label does not
validate`) is a count and never a name, because the name is the string that
failed validation. It is a tenant this warden can see and can never address:
no roll will converge it, now or ever. Find it with
`kubectl -n tenants get secret -l app.kubernetes.io/managed-by=squelch-warden
-o name | grep -- -identity` and compare against `squelch-control tenants`.
Either the validation rules tightened under a real tenant — which is a mailbox
stuck on whatever render it has, and wants a hand-driven fix — or it is junk
somebody applied, and wants deleting.

### Alerting on this, without alerting on normal

Anything other than 0 marks the Job **failed**, because Kubernetes knows zero
and non-zero and nothing else. That means **a fleet mid-roll leaves failed Jobs
in its history by design**: nine tenants behind is nine failed Jobs and then a
green one, every image bump. An alert on `kube_job_status_failed{namespace="warden"}`
alone will page on every ordinary rollout and be ignored inside a week.

**The exit code itself is not in metrics, and cannot be.** kube-state-metrics
builds `kube_pod_container_status_last_terminated_exitcode` from a container's
`lastState` - the state it was in before its LAST RESTART. A roll pod runs once
and exits, so its code lands in `state.terminated`, `lastState` stays `{}`, and
no metric anywhere carries a current terminated state's exit code. The query
this section recommended until 2026-08-25 -
`kube_pod_container_status_last_terminated_exitcode{namespace="warden",container="roll"} == 4` -
therefore matched nothing, on any cluster, on any day, including every day a
casualty would have frozen the fleet. Checked against production: that metric
had exactly ONE series in the whole cluster, belonging to a `helm-install` pod
that happened to retry.

**The Job's failure REASON is exported, so that is what carries the signal.**
`90-warden-roller.yaml` gives the jobTemplate a `podFailurePolicy` whose single
rule fails the Job on exit code 4. The Job controller then writes
`reason: PodFailurePolicy` on that Job's Failed condition, where every other
nonzero exit gets `reason: BackoffLimitExceeded`. **Page on this:**

```promql
count(
  (kube_job_status_failed{namespace="warden", reason!~"BackoffLimitExceeded|DeadlineExceeded|Evicted"} > 0)
  and on(namespace, job_name)
  (time() - kube_job_status_start_time{namespace="warden"} < 3600)
) > 0
```

Three parts, each load-bearing:

- **The negation, rather than `reason="PodFailurePolicy"`.** kube-state-metrics
  hardcodes `jobFailureReasons = {BackoffLimitExceeded, DeadlineExceeded,
  Evicted}` and files every other reason under the empty string - which
  Prometheus drops on ingest, so a casualty arrives with no `reason` label at
  all. The negation matches that series today AND keeps matching on the day
  upstream adds `PodFailurePolicy` to its list and the label appears for real.
  Do not tidy this into an equality match; it would be correct for one KSM
  release and silent after it.
- **`> 0`.** Every roll Job publishes a reason-less series worth 0 while it is
  healthy, and that series matches the negation too. The comparison is the whole
  difference between a casualty and a green run.
- **The one-hour window.** A failed Job object survives until 24 more failures
  push it out of history, so an unwindowed query stays red for days after the
  fleet is fixed - the same trap that had "Pods not ready" reading 3 over
  yesterday's corpses. The window costs nothing, because a frozen fleet
  RE-RAISES the casualty on every tick: the read pass halts before writing on
  every run, so the signal renews itself every five minutes for as long as
  the problem is real, and goes cold within an hour of being fixed.

That is the frozen fleet: nothing converged, and nothing will until a person
acts. Everything else belongs on a board.

Sixty seconds of probe proves the whole path - Job controller, kube-state-metrics,
remote_write, Prometheus - without waiting for a real casualty:

```sh
kubectl create ns roll-exit-probe
for CODE in 3 4; do kubectl apply -f - <<EOF
apiVersion: batch/v1
kind: Job
metadata: {name: probe-exit$CODE, namespace: roll-exit-probe}
spec:
  backoffLimit: 0
  podFailurePolicy:
    rules:
      - action: FailJob
        onExitCodes: {containerName: roll, operator: In, values: [4]}
  template:
    spec:
      restartPolicy: Never
      containers:
        - {name: roll, image: busybox:1.36, command: ["/bin/sh","-c","exit $CODE"]}
EOF
done
# exit 3 -> BackoffLimitExceeded, exit 4 -> PodFailurePolicy
kubectl -n roll-exit-probe get job -o jsonpath='{range .items[*]}{.metadata.name}{"  "}{.status.conditions[?(@.type=="Failed")].reason}{"\n"}{end}'
# then the query above with namespace="roll-exit-probe" should count exactly 1
kubectl delete ns roll-exit-probe
```

The split matters more than it looks, because codes 1, 2 and 3 can all be
**permanent**. A stranded mailbox raises 1 every five minutes until somebody
reconciles it; an unreadable label raises 2 forever; a stalled queue raises 3
forever. Each of those is a state an operator can reasonably look at, decide to
live with for a week, and stop reading. If the casualty shared a code with any
of them, the one signal meaning *the fleet has stopped converging entirely*
would be muted by a tenant nobody is worried about, and the next release would
reach nobody, silently.

So: **4 pages.** 1 goes on a board and gets triaged — treat a *new* label
appearing in it as worth a look, and expect the old ones to persist. 2 is a
weekly cleanup. 3 is healthy unless its `still behind` count stops falling
across consecutive runs, which is the stall signature and worth its own alert:

```promql
min_over_time(BEHIND[2h:5m]) > 0
  and
(max_over_time(BEHIND[2h:5m]) - min_over_time(BEHIND[2h:5m])) == 0
```

where `BEHIND` is the fleet's own convergence gap, the expression behind the
dashboard's "Off the majority image":

```promql
count(kube_pod_container_info{namespace="tenants", container="squelchd"})
  - max(count by (image_spec) (kube_pod_container_info{namespace="tenants", container="squelchd"}))
```

Read it as "the fleet has been behind for two hours and the number never once
went down". A healthy roll drains one tenant per tick, so eight ticks with no
movement at all is a queue that is not draining rather than a queue that is
long. This replaces a second query that died with the first one - it too read
the exit code out of a metric that was never emitted.

**Exit 64 pages through neither of these**, because a mistyped `args:` fails the
Job the ordinary way and the fleet simply stops converging in silence. What
catches it is the timer itself going quiet:

```promql
time() - max(kube_cronjob_status_last_schedule_time{namespace="warden"}) > 4200
```

Seventy minutes rather than twenty, because `concurrencyPolicy: Forbid` means a
run that overruns its tick legitimately delays the next schedule, and
`activeDeadlineSeconds` allows an hour of that. Both this and the stall query
are on the dashboard's "Daemon rollout" row.

```sh
kubectl -n warden get cronjob squelch-warden-roll         # last schedule, ACTIVE, suspended or not
kubectl -n warden get jobs                                # one row per run, 3 kept + 24 failed
kubectl -n warden logs job/<name>                         # per-tenant lines, then the summary
kubectl -n warden patch cronjob squelch-warden-roll -p '{"spec":{"suspend":true}}'
```

**Neither of those last two is what it looks like.** `suspend: true` stops the
NEXT tick and does nothing to a run already going — that one keeps applying, one
mailbox at a time, and stopping it means deleting its Job
(`kubectl -n warden delete job <name>`, which takes the pod with it; the tenants
already rolled stay rolled and the rest wait for the next run). And
`kubectl -n warden create job --from=cronjob/squelch-warden-roll roll-now` makes
a **standalone** Job that `concurrencyPolicy: Forbid` does not count, so it can
walk the fleet beside a scheduled tick, at a different offset, with two mailboxes
down at once. Suspend first, confirm nothing is active, create, then clean up and
unsuspend:

```sh
kubectl -n warden patch cronjob squelch-warden-roll -p '{"spec":{"suspend":true}}'
kubectl -n warden get cronjob squelch-warden-roll -o jsonpath='{.status.active}{"\n"}'   # empty
kubectl -n warden create job --from=cronjob/squelch-warden-roll roll-now
kubectl -n warden logs -f job/roll-now
kubectl -n warden delete job roll-now      # nothing else collects it, and the name is taken
kubectl -n warden patch cronjob squelch-warden-roll -p '{"spec":{"suspend":false}}'
```

**A run that halts on the SAME label every tick is a render the cluster refuses,
not a flaky tenant.** A rejected apply writes nothing, so that tenant is still
drifted, so it is first in the queue again five minutes later and rejected
again — ~288 failed Jobs a day and not one tenant converged in any of them. Unlike
a casualty, this one never moves on its own.

A reconcile applies a tenant's PVC, NetworkPolicy and Service before its
Deployment, so the object the API server refuses is often not the thing that
queued the tenant: raise `SQUELCH_WARDEN_STORAGE_SIZE` in the same edit as an
image bump, on a storage class with no `allowVolumeExpansion`, and every drifted
tenant dies at `volume_failed` on a PVC the image bump had nothing to do with.
The machine reason in the log names the object (`volume_failed`,
`network_policy_failed`, `service_failed`, `workload_failed`, `ingress_failed`,
and `render_rejected` for a render the dry run refused before anything was
applied). Suspend, put back whatever changed in `15-warden-config.yaml`, restart
the warden, unsuspend. `squelch-control reconcile <label>` on that one tenant
reproduces the refusal with the API server's own message when the reason word is
not enough. `failedJobsHistoryLimit: 72` is what keeps six hours of that evidence
readable instead of half an hour of it.

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

This is the shape a reconcile the CLI hung up on leaves behind, and the roller
naming it is often the first anyone hears of it — the operator who ran the
command was told the provisioning service could not be reached and had no reason
to look. See "Shipping a tenant-shape change" above for why the client's answer
is not the record and what the two failure messages mean.

A closed account never appears here, and the SERVICE is why: a teardown takes
it before the Deployment, so a tenant with no workload and no Service is a
cancellation (marked or, for the ones that predate the marker, inferred) and
lands under "no workload to converge" instead. That distinction is load-bearing
— naming a cancelled account as DOWN would send you to the `reconcile` that
puts it back on the internet.

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

### Turning on the HTTP readiness probe

A knob with an order attached, and the order is the whole warning.

`SQUELCH_WARDEN_HTTP_READINESS` switches every tenant's readiness probe from a
TCP accept on 8848 to an HTTP GET of `/healthz` on 9464. It is worth having: the
daemon binds its listeners before it finishes starting (on purpose — a first-run
model download must not leave the doors unreachable), so an accept calls a tenant
Ready about two seconds in, and calls one Ready that died on the way up straight
after the bind. `/healthz` answers 200 only once the daemon has finished
starting, and 503 again if its sync task comes apart afterwards.

**It does not catch a dead credential**, and that is deliberate rather than a
gap. The daemon's sync loop retries a token Google has stopped accepting rather
than giving up, so the task stays alive and `/healthz` stays 200. Answering 503
there would pull the pod out of its own Service — taking away the one door its
owner would use to re-consent — and the roller would read the tenant as a
casualty and stop converging the fleet. A rejected credential is an alert on the
metrics next door, not a readiness state.

**Two prerequisites, and both of them take tenants down if you skip them.**

*Only a daemon that ships the route serves it.* A tenant on an image that
predates it fails an HTTP probe on every period, never reports Ready, is pulled
out of its own Service, and halts the next roll. Turning this on before the fleet
is converged therefore takes every tenant that is behind DOWN, one after another,
and none of them come back until the knob goes off again.

*And `/healthz` waits out the first-run model download.* It answers 503 until the
daemon's background embedder init has settled, which on a cold weights cache
means ~126 MB from Hugging Face — longer than
`SQUELCH_WARDEN_READY_TIMEOUT_SECS` (default 180). Each tenant caches those
weights on its own volume, so this bites the FIRST boot of any tenant: a new
signup gets `500 not_ready` for a mailbox that is perfectly healthy, and the next
roll reads that same tenant as a casualty and stops the whole fleet.
`SQUELCH_WARDEN_MODEL_PVC` is what removes the download (every tenant's init
container copies from a pre-seeded PVC instead). It ships commented out in
`15-warden-config.yaml` and SETUP.md step 10 is what turns it on, in that order:
the volume, then the seed, then the knob. Set ahead of the volume it is worse
than unset, because a name that resolves to no claim leaves every new tenant pod
Pending. And the value is only as good as the volume behind it, so confirm both
ends rather than the variable alone. The warden logs a warning at startup if this
knob is on and that one is unset.

```sh
kubectl -n tenants get pvc squelch-models
kubectl -n warden exec deploy/squelch-warden -- printenv SQUELCH_WARDEN_MODEL_PVC

# What is ON the volume, while the seed pod still exists. Once it is deleted the
# check is a tenant's own cache instead: SETUP.md step 10, "Verify on the next
# tenant".
kubectl -n tenants exec squelch-models-seed -- ls /seed
```

So, in order, with a roll between each:

1. Bump `SQUELCH_WARDEN_IMAGE` to a daemon that serves `/healthz`, apply, restart
   the warden — the three steps at the top of this section.
2. Let the roller converge, and CHECK it did — a clean `roll --dry-run`, or
   `kubectl -n tenants get deploy -o jsonpath` over the images. Every tenant, not
   most of them: the ones left behind are exactly the ones the next step breaks.
3. Create and seed the volume, THEN uncomment `SQUELCH_WARDEN_MODEL_PVC` and
   restart the warden, and confirm it came up with the variable set (SETUP.md
   step 10 is that whole sequence and the order is not negotiable). Or raise
   `SQUELCH_WARDEN_READY_TIMEOUT_SECS` past a cold model download and accept a
   slow first provision. Skipping this does not break the tenants you have; it
   breaks the next one that signs up.
4. Set `SQUELCH_WARDEN_HTTP_READINESS: "on"` in `15-warden-config.yaml`, apply,
   restart the warden, and let the roller converge again. Each tenant's pod
   restarts once more, because the probe is part of the pod spec.

Backing out is the same edit in reverse and one more roll; nothing about it is
one-way. And if a tenant does get stranded mid-sequence, the fix is to converge
it onto the newer daemon rather than to wait: `squelch-control reconcile <label>`.

`SQUELCH_WARDEN_MIN_READY_SECS` (default 30) needs no sequence. It is a
`minReadySeconds` on the tenant Deployment, so a replica is not Available until
it has stayed Ready that long, and the roller waits for Available. It applies to
every daemon image, costs half a minute per tenant on a roll, and has to stay
below `SQUELCH_WARDEN_READY_TIMEOUT_SECS` — the warden refuses to boot otherwise,
because a soak the rollout wait cannot outlast would time out on every healthy
tenant.

### Reclaiming the second copy of the model (one-off, run deliberately)

**Nothing applies this. It is a `rm -rf` inside a tenant's mail volume, typed by
a person who has first established which build that tenant is loading.** It buys
63 MB per tenant, which is worth having on a 4-tenant box and is not worth being
casual about. Establishing that is the part with a dependency; see the gate
below before deleting anything.

Until the model was pinned, the daemon resolved its embedding model by substring
match over fastembed's supported list, and which of the two `bge-small-en-v1.5`
builds won was not stable across versions. Every tenant volume provisioned in
that window therefore holds BOTH:

```
/data/.local/share/squelch/models/
├── models--Xenova--bge-small-en-v1.5          126 MB   <- the pinned one
└── models--Qdrant--bge-small-en-v1.5-onnx-Q    63 MB   <- dead weight
```

Only one of them is ever loaded. Deleting the other is safe, once the daemon on
that tenant is actually loading the pinned one.

**This step has a dependency, and today it is not met.** The gate you want is
the daemon naming the model it actually loaded, which is the `squelch: embedding
model <code> (<dim>-dim) loaded` line that arrives with the model pin. Until that
build is what the fleet runs, there is no such line: the only thing a daemon
prints about the model today is the first-run download notice, which echoes the
CONFIG string rather than the resolved code and is suppressed entirely once the
cache is warm. So `grep 'embedding model'` returns nothing on every tenant, and
restarting to read a fresh init returns the same nothing. Do not read that
silence as a verdict.

With that line shipped, this is the check and nothing else is needed:

```sh
kubectl -n tenants logs deploy/<label> -c squelchd | grep -i 'embedding model'
# squelch: embedding model Xenova/bge-small-en-v1.5 (384-dim) loaded
```

Until then the gate is two facts, both about configuration rather than about
what got loaded, and BOTH have to hold before anything is deleted:

```sh
# 1. This tenant is on a daemon that carries the pin. Two parts, and the second
#    is the one that gets skipped: the warden's configured tag has to BE a build
#    with the pin in it, and the Deployment that is serving has to be on that
#    tag rather than behind it.
kubectl -n tenants get deploy/<label> \
  -o jsonpath='{.spec.template.spec.containers[0].image}{"\n"}'
kubectl -n warden get cm squelch-warden-config \
  -o jsonpath='{.data.SQUELCH_WARDEN_IMAGE}{"\n"}'

# 2. Nothing in this tenant's own configuration names the quantized build. The
#    warden renders no embed model into the pod, so the daemon runs on the
#    default unless a config.toml on the volume says otherwise, and `$HOME` is
#    /data. `exec` runs no shell, so a glob or a missing file needs one.
kubectl -n tenants exec deploy/<label> -c squelchd -- \
  sh -c 'cat /data/.config/squelch/config.toml 2>/dev/null | grep -i model || echo "no config.toml"'

# And what the two directories cost, so the 63 MB is a number you have seen.
kubectl -n tenants exec deploy/<label> -c squelchd -- \
  sh -c 'du -sh /data/.local/share/squelch/models/*'
```

If the tenant is behind on the image, STOP and roll it forward first
(`squelch-control reconcile <label>`): on a pre-pin daemon the build being
loaded is a coin flip per process, so the Qdrant directory may be the one in
use, and deleting it costs that tenant a 126 MB download on its next restart. If
a `config.toml` names the Qdrant code, that tenant is deliberately on it and this
section does not apply to it at all.

Then, one tenant at a time:

```sh
kubectl -n tenants exec deploy/<label> -c squelchd -- \
  rm -rf /data/.local/share/squelch/models/models--Qdrant--bge-small-en-v1.5-onnx-Q

kubectl -n tenants exec deploy/<label> -c squelchd -- \
  ls /data/.local/share/squelch/models
# models--Xenova--bge-small-en-v1.5
```

No restart is needed for THIS deletion: what goes is the build nothing loads, and
the pinned directory beside it is untouched. The next restart re-reads that copy,
and the init container will not re-seed the deleted one, because the PVC does not
carry it either.

Do not generalise it into "a running daemon never touches this directory again".
That is true of the daemon on the box today, which reads its weights at init and
holds the session for the life of the process, and it stops being true with the
idle-unload work in flight: that one drops the session after a quiet window and
RELOADS it from this same directory on the next search or poll tick. The pinned
model's directory is live for as long as the pod is, so the only thing ever safe
to delete under a running daemon is a build it is not using.

Do the four existing tenants and then forget this section. Tenants provisioned
after the pin only ever fetch one model.

**The same `rm -rf` is the repair for a CORRUPT model directory**, which is the
one case the init container cannot fix itself. It seeds by copying to
`<model>.seed-tmp` and renaming, so a copy it started and did not finish leaves
no destination and is retried on the next boot. A directory half-populated under
the model's real name, by a download the daemon itself did not finish, is
indistinguishable from a good one: the seeder sees it exists and skips it,
forever. The symptom is a tenant that never reaches `embedder ready`, or reaches
it and then errors on the first embed with a missing file under `snapshots/`.

```sh
kubectl -n tenants exec deploy/<label> -c squelchd -- \
  sh -c 'ls -lLR /data/.local/share/squelch/models/models--Xenova--bge-small-en-v1.5/snapshots'

# Broken links, or no blobs/ at all: delete the directory and restart. The init
# container re-seeds it from the PVC, or the daemon re-downloads it.
kubectl -n tenants exec deploy/<label> -c squelchd -- \
  rm -rf /data/.local/share/squelch/models/models--Xenova--bge-small-en-v1.5
kubectl -n tenants rollout restart deploy/<label>
```

**Delete and restart, as one action.** This is the pinned model, so it is the
directory a running daemon loads from, and the restart is what puts it back:
between the two commands the pod has no weights, and with idle unload shipped a
reload landing in that window fails until the new pod's init container has
re-seeded. Do not delete this one and walk away.

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

### Letting a tenant invite their own friends

**A tenant provisioned from now on can already share.** Signup mints the token
and installs it in the same window the LLM keys go in, BEFORE the workload is
applied, so the pod's first render carries it and nothing rolls twice to pick it
up. It is fail-soft: every way it can go wrong leaves a mailbox that works and
cannot share, which is what the command below is for.

**Every tenant provisioned BEFORE it needs the command**, one at a time, the
same shape `llm mint` has:

```sh
railway ssh --service control
squelch-control share mint <label>      # mint, record the hash, install it
squelch-control share revoke <label>    # forget the hash, pull it from the pod
```

`share mint` mints a share token, records only its SHA-256 against the tenant
row, and PUTs the plaintext to the warden, which writes it into a
`<label>-control` Secret and rolls the pod so the daemon picks up
`SQUELCH_CONTROL_TOKEN`. The token is never printed: nothing needs to read it
but the pod.

With it installed the tenant's own Passband offers a share sheet, and each
invite it sends is **mailed by the user's own Gmail, not by Resend** — the
control plane mints the code and is never told who it was for. The cap is 10
codes per tenant per rolling 30 days, counted from `invite_codes.invited_by`
rather than stored, so changing the limit takes effect at once.

**Both halves of a revoke matter.** Clearing the hash stops the token working
immediately, whatever the pod still holds; pulling it from the pod is what stops
the app OFFERING a button whose every press would be refused. `share revoke`
does both, in that order, so an interrupted run has already done the part that
protects the mint.

`invite list` shows codes minted this way alongside operator-minted ones. The
tenant behind each is `invite_codes.invited_by`, which is the referral funnel and
the only thing the control plane records about a share: **not** the recipient,
who has consented to nothing and whose address never leaves their friend's
daemon.

A tenant with no share token boots fine and reports `invite_sharing: false` on
`/client/stats`, so the app shows no button and the two-week nudge never fires.
Same for every self-hosted daemon, which has no control plane to mint against at
all.

Signup's mint is gated on the invite policy being configured, the same condition
`POST /tenant/invite` itself answers 503 without: a deployment with no invite
feature installs no credential for a door that cannot open.

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
  covered: the roller (see "Rolling the daemon image") walks the fleet every 5
  minutes and fixes what it can. What is missing is anybody finding out when it
  cannot. A halt or a foreign-drift skip is a failed Job in ns `warden` and
  nothing else — no email, no dashboard panel, and `failedJobsHistoryLimit: 72`
  runs, six hours at this schedule, before the evidence rotates away.
  kube-state-metrics is already scraped off this box, so the alert is
  `kube_job_status_failed{namespace="warden"} > 0` plus a panel; it wants
  writing, not inventing. The history limit buys reading time; it is not the
  alert.

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
- **A reconcile is still tied to the caller's connection** (issue #91). The
  warden runs the whole delete-wait-apply-wait inside the request handler, so a
  client that hangs up takes the work with it: axum drops the handler future and
  the warden stops mid-operation, possibly with a tenant's Deployment already
  deleted. The ten-minute client budget and the honest timeout message make that
  much less likely and much more legible, but neither one makes the warden
  finish. The fix is a `202` with a status route to poll, or a handler the
  runtime does not drop when the connection goes; it changes the wire shape and
  wants its own design pass. Until then, nothing about the warden's job should
  depend on whether an operator's terminal is still listening, and it does.
- **`/mcp` bearer auth** — the hosted MVP routes around it (tenant Ingresses
  publish `/client`, `/console` and `/t` only). Real auth is required before the
  agent door is ever served from the internet, and until then the console's MCP
  section says so instead of offering a URL that 404s.
- **Google verification + CASA Tier 2** — the restricted-scope cap is 100 users.
  This gates user 101 and has the longest lead time of anything on this list.
  The submission, its blockers, and the assessment scope are
  `docs/VERIFICATION.md`.
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
