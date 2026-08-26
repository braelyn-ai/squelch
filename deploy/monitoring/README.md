# Hosted monitoring

Two halves, split by one rule: **nothing that judges carrier's health may live
on carrier.**

## The Railway half (this directory)

Three services in the existing `squelch` Railway project, each built from a
root-level Dockerfile and its own config-as-code file (the `railway.*.toml`
lesson from `deploy/DEPLOY.md` §8 applies — the file path is set in service
settings, `RAILWAY_DOCKERFILE_PATH` is ignored):

| Service | Image base | Config | Public domain |
|---|---|---|---|
| `prometheus` | `prom/prometheus:v3.5.0` | `railway.prometheus.toml` | yes — carrier pushes here (port 9090) |
| `blackbox` | `prom/blackbox-exporter:v0.25.0` | `railway.blackbox.toml` | **never** — private networking only |
| `grafana` | `grafana/grafana:12.2.0` | `railway.grafana.toml` | yes — the dashboard (port 3000) |

Prometheus stores 30 days on its volume, receives `remote_write` from carrier
(basic auth, every route), and scrapes blackbox probes of
`signup.passband.app/healthz`, `warden.passband.app/healthz` and
`passband.app` — so uptime is measured from outside the box.

Grafana provisions its datasource and the `passband-health` dashboard from
files baked into the image. Edit the dashboard by editing
`grafana/dashboards/passband-health.json` and **merging it to `main`**, not in
the UI — UI edits die with the next deploy (`allowUiUpdates: false` says so out
loud).

All three of these services deploy themselves when `main` moves; they are wired
to this repo with a deployment trigger on `main`, so a merge is the deploy.
(This paragraph used to say `railway up -s grafana`, which sent at least one
person down a staging-dir rabbit hole to do by hand what a merge does on its
own. `railway up` uploads your working tree, so it can put a build into
production that exists in no commit, and from the repo root it is silently
SKIPPED on these services. See the root `CLAUDE.md`.)

Because the dashboard ships inside the image, **merging a panel change is not
the same as seeing it** — the service has to finish rebuilding. Check the thing
itself rather than the deploy status:

```sh
railway ssh --service grafana -- grep -c last_over_time \
  /etc/grafana/dashboards/passband-health.json
```

Credentials: `PROM_REMOTE_WRITE_PASSWORD` (plain) + `PROM_WEB_BCRYPT` (its
bcrypt) on prometheus; the same plain value as `PROM_PASSWORD` on grafana,
plus `GF_SECURITY_ADMIN_PASSWORD`. All live in Railway variables and in the
`monitoring/remote-write-auth` Secret on carrier — nowhere else.

## The carrier half

`deploy/hosted/80-monitoring.yaml`: node-exporter, kube-state-metrics, and a
Prometheus in **agent mode** that scrapes node-exporter, kubelet, cadvisor,
kube-state-metrics, Traefik and cert-manager, then pushes what survives
relabeling. No inbound port; the firewall stays 22/80/443. It is meant to be a
small tenant of the box and has to be kept that way on purpose; see below.

### cadvisor and kubelet are allow-listed by metric name

Those two endpoints are the cardinality hogs, and the agent is what pays for
them: a 213 MiB working set against a 256Mi limit on a 3.8 GB box that has
already global-OOM-killed tenant daemons. (How much of that 213 was series and
how much was page cache over the agent's WAL is unmeasured; the agent now
scrapes itself, so the next answer is a measurement.) So once the config below
is applied they are not scraped whole: cadvisor keeps five families, drops the
`id`, `name` and `image` labels, and drops the container-less rollups; kubelet
keeps `kubelet_volume_stats_*` and nothing else. Everything else the dashboard
reads with a `container_` in the name (`kube_pod_container_info`,
`kube_pod_container_status_*`) is kube-state-metrics, not cadvisor, and that
job is still scraped in full.

**Adding a panel on a cadvisor or kubelet family that is not in the keep regex
gets you NO DATA, not a slow query.** The fix is to widen the regex in the job's
`metric_relabel_configs` in `80-monitoring.yaml`. The comment above each job
says which names are in and why one is deliberately out.

Two things bite here. First, `up` and the other synthetic scrape series are
appended after metric relabeling, so a broken keep regex looks like a healthy
target serving nothing, not like a dead target. That is what the third line on
the dashboard's **Unhealthy signals** panel watches for: it counts cadvisor and
kubelet targets that are either down or reporting zero samples past relabeling,
and it should sit flat at zero. Second, **applying the ConfigMap does not
reload the agent**: it runs without `--web.enable-lifecycle` and reads its
config once, at boot. Carrier served a 13-day-stale Prometheus config exactly
that way, and every per-tenant panel was blank while nothing looked wrong.

```sh
kubectl apply -f deploy/hosted/80-monitoring.yaml
kubectl -n monitoring rollout restart deploy/prometheus-agent
```

To see what a target actually offers before guessing at a name, dump the
families off the endpoint through the API server's node proxy, which spares you
the service-account token, the TLS flags and a shell inside the agent's
container:

```sh
NODE=$(kubectl get node -o jsonpath='{.items[0].metadata.name}')
kubectl get --raw "/api/v1/nodes/$NODE/proxy/metrics/cadvisor" \
  | grep '^# TYPE' | awk '{print $3}'
```

Swap `/metrics/cadvisor` for `/metrics` to do the same for the kubelet's own
families.

The agent scrapes itself on a job of its own (`prometheus-agent`, under ten
series), because the allow-lists are a memory argument and it should be possible
to read whether they worked. On the Railway Prometheus:

```promql
prometheus_agent_active_series{cluster="carrier"}
process_resident_memory_bytes{cluster="carrier", job="prometheus-agent"}
```

The two do not track each other: resident memory includes page cache over the
agent's WAL under `/data`, so a series cut moves the first immediately and the
second only as the cache turns over. `prometheus_target_scrapes_sample_out_of_order_total`
is on the same job, and it is where a relabel rule that collapses two series
into one shows up. From the box, the resident half of that, without Grafana:

```sh
kubectl -n monitoring top pod -l app.kubernetes.io/name=prometheus-agent
```

## What the dashboard answers

- **Is it up?** — external probes of signup/warden/landing, TLS days left,
  the box still reporting, pods not ready.
- **carrier** — CPU (incl. **steal**, the shared-vCPU tax), memory, both
  disks (root = backed up, tenant volume = not), load, network.
- **Kubernetes** — restarts, top pods by CPU/memory, OOM kills.
- **Certificates & edge** — cert-manager expiry (wildcard renewal failing
  shows here first), Traefik request/5xx/latency.
- **Tenants** — pod count vs the 100-user cap, per-tenant CPU/memory/PVC/restarts.
- **Daemon rollout** — which image version each tenant pod runs, and the
  switchover as it happens, plus the roller's own two vital signs: a casualty
  (a roll that exited 4, the fleet frozen, the one thing worth a page) and how
  long since the timer last fired. See below.
- **Inside squelchd** — sync staleness per tenant, Gmail API errors by kind,
  24h LLM spend, store size, triage throughput.

## Watching a release roll

Releases are rolling: the warden repoints one tenant Deployment at a time, so
for a while the fleet runs two versions at once. The **Daemon rollout** row is
that window, told twice over.

The version a panel means is the image tag with its `daemon-` prefix stripped,
so `ghcr.io/braelyn-ai/squelchd:daemon-0.4.0` reads as `0.4.0` — the same
spelling `squelchd_build_info` uses, which is what lets the two sides be
compared. A digest-pinned image has no tag to strip and reads as its digest.

Two sources, and the gap between them is the point:

- **kube-state-metrics** (`kube_pod_container_info`) is what Kubernetes was
  *told* to run. It moves the instant a Deployment is patched, whether or not
  the pod ever starts.
- **the daemon itself** (`squelchd_build_info`, off port 9464) is what is
  *actually running*. It lags by however long a pull and a start take.

So "Pods by image version" leads and "Pods by daemon version" follows, and a
gap that does not close is a pod holding the new image it cannot run. **Ahead
of the daemon** counts exactly those, and deliberately counts only tenants
whose daemon exports metrics at all, so it reads 0 rather than crying wolf on
tenants not yet re-applied with `SQUELCH_METRICS_BIND`.

The tenant label on the kube-state-metrics side is derived from the pod name,
because kube-state-metrics does not carry pod labels unless allowlisted. A pod
is named `<deployment>-<replicaset>-<pod>` and the warden names a Deployment
after the tenant, so the tenant is the pod name minus its last two hyphen
segments. Tenant labels may contain hyphens themselves, which is why the regex
takes from the end.

Reading the row during a release:

| Panel | Converged | Rolling | Wrong |
|---|---|---|---|
| Image versions live | 1 | 2 | 3+, or stuck at 2 |
| Off the majority image | 0 | counting down | stops falling |
| Image pull failures | 0 | 0 | any — bad tag or pull secret |
| Ahead of the daemon | 0 | brief, per pod | sticky — crash loop on the new build |

**Version by tenant** is the per-tenant answer: one band per tenant and
version, so you can see which tenants have gone over and which have not. The
gap between a tenant's two bands is its downtime for the release: a tenant
Deployment is `Recreate`, not `RollingUpdate`, because its SQLite store takes
exactly one writer, so the old pod is gone before the new one starts. That is
also why the stacked totals dip by one at each handoff rather than bulging.
A gap that does not close is a pod that never came back.

**Tenant image inventory** is the same thing as a list, plus pod age (how long
ago that tenant rolled) and the resolved digest, which is the only column that
can tell two pulls of the same mutable tag apart.

A dashboard-wide annotation marks the moment a version nothing ran ten minutes
ago first appears on a pod, so the roll is a vertical line across every other
panel too — which is how you tell "the restarts at 14:05" from "the release at
14:04".

## The daemon's own metrics

Every tenant daemon runs with `SQUELCH_METRICS_BIND=0.0.0.0:9464` (set by the
warden) and serves Prometheus text on container port `metrics`, named
`squelchd_*`. `prometheus-agent` discovers those pods in the `tenants`
namespace by `app.kubernetes.io/name=squelchd` plus the port name, relabels
`app.kubernetes.io/instance` to `tenant`, and caps each scrape at 500 samples.

That listener has no authentication, so reaching it is a network question and
the answer is one rule: the per-tenant NetworkPolicy admits the `monitoring`
namespace's `app.kubernetes.io/name=prometheus-agent` pod to 9464 and nothing
else. Port 9464 is on the pod only — not on the tenant Service, not in its
Ingress — so it cannot be published through a tenant vhost by any routing
mistake, and the agent cannot reach 8848 (mail) from that allowance.

The alert worth having, which is the thing external probes structurally cannot
see (a pod can be Running, ready and stuck since Tuesday):

```promql
time() - squelchd_sync_last_success_timestamp_seconds > 900
```

One tenant firing is usually a dead refresh token (its
`squelchd_gmail_api_errors_total{kind="auth"}` will be climbing) and that tenant
has to re-consent; every tenant firing at once is the box.

A first backfill that ran without the embedder (the init never settled inside
its three-minute ceiling) is `increase(squelchd_embedder_gate_timeouts_total[1h])
> 0`. The mailbox is fine; it is the memory profile that is not, because the
vector backfill pass then drains that mailbox in batches instead of ingest
embedding it one message at a time. One is worth a look; a train of them is a
wedged init retrying.
