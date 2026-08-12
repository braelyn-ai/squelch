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
`grafana/dashboards/passband-health.json` and redeploying (`railway up -s
grafana`), not in the UI — UI edits die with the next deploy
(`allowUiUpdates: false` says so out loud).

Credentials: `PROM_REMOTE_WRITE_PASSWORD` (plain) + `PROM_WEB_BCRYPT` (its
bcrypt) on prometheus; the same plain value as `PROM_PASSWORD` on grafana,
plus `GF_SECURITY_ADMIN_PASSWORD`. All live in Railway variables and in the
`monitoring/remote-write-auth` Secret on carrier — nowhere else.

## The carrier half

`deploy/hosted/80-monitoring.yaml`: node-exporter, kube-state-metrics, and a
Prometheus in **agent mode** that scrapes node-exporter, kubelet, cadvisor,
kube-state-metrics, Traefik and cert-manager, then pushes everything out.
No inbound port; the firewall stays 22/80/443. Total footprint ~200 MB.

## What the dashboard answers

- **Is it up?** — external probes of signup/warden/landing, TLS days left,
  the box still reporting, pods not ready.
- **carrier** — CPU (incl. **steal**, the shared-vCPU tax), memory, both
  disks (root = backed up, tenant volume = not), load, network.
- **Kubernetes** — restarts, top pods by CPU/memory, OOM kills.
- **Certificates & edge** — cert-manager expiry (wildcard renewal failing
  shows here first), Traefik request/5xx/latency.
- **Tenants** — pod count vs the 100-user cap, per-tenant CPU/memory/PVC/restarts.
- **Inside squelchd** — sync staleness per tenant, Gmail API errors by kind,
  24h LLM spend, store size, triage throughput.

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
