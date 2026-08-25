# Working in this repo

## Deploys: push to `main`, do not run `railway up`

**Every Railway service that builds from this repo deploys itself when `main`
moves.** They are wired to the GitHub repo with a deployment trigger on `main`,
so the way to ship a change to any of them is to merge it. Running `railway up`
instead is a worse version of the same thing: it uploads whatever is on your
disk, which is how a service ends up running a build that exists in no commit.

| Service | Deploys from | Config-as-code file |
|---|---|---|
| `control` | `main` | `railway.control.toml` |
| `grafana` | `main` | `railway.grafana.toml` |
| `prometheus` | `main` | `railway.prometheus.toml` |
| `blackbox` | `main` | `railway.blackbox.toml` |
| `landing-site` | `main` | (root dir `/passband-site`) |
| `squelch` (relay) | `main` | (root `railway.toml`) |
| `bifrost` | **nothing** — no GitHub trigger | `railway.bifrost.toml` |

`bifrost` is the one genuine exception: it has no trigger, because it builds
from a FROM-only Dockerfile pinning an upstream image rather than from our
source. Deploying it is the staging-dir `railway up` documented in
`railway.bifrost.toml`.

Two things worth knowing before you push:

- **The triggers do not wait for CI** (`checkSuites: false`). A merge to `main`
  starts a build immediately, whether or not the Rust job has passed. Green CI
  on the PR before merging is the gate; there is no second one.
- **A `railway up` from the repo root is silently SKIPPED** on several
  services: the deployment appears in the list as SKIPPED, no build runs, and
  the previous image keeps serving, so a change looks deployed and is not. This
  is the trap that makes `railway up` worse than useless as a habit rather than
  merely redundant.

The legitimate uses of `railway up` are narrow: `bifrost`, and deploying a
branch to production deliberately without merging it. If you reach for it for
any other reason, merge instead.

### Grafana specifically

The dashboard is not editable in the UI in any way that survives. It is baked
into the image (`Dockerfile.grafana` copies
`deploy/monitoring/grafana/dashboards/` to `/etc/grafana/dashboards`) and
provisioned with `allowUiUpdates: false`. So a dashboard change is a change to
`passband-health.json`, merged to `main`, and the service rebuilds itself.

Verify what actually shipped rather than trusting the deploy status:

```sh
railway ssh --service grafana -- grep -c last_over_time \
  /etc/grafana/dashboards/passband-health.json
```

## Verifying against production

Read state from the real services before believing a doc about them. Two
findings from one afternoon in August 2026, both from docs that were true when
written:

- `deploy/monitoring/README.md` said to redeploy Grafana with `railway up -s
  grafana`. Grafana had been auto-deploying from `main` for some time.
- `deploy/hosted/SETUP.md` §6b said to set the Bifrost provider key's models to
  `claude-haiku-4-5, claude-sonnet-5`. The fleet had moved to `claude-opus-5`
  and every hosted tenant's triage had been failing for four days.

The Railway CLI is authenticated here, so the current answer is usually one
command away: `railway variables -s <svc> --json` (the table view TRUNCATES
values, which has hidden a bug at least once), `railway ssh --service <svc> --
<cmd>`, and the GraphQL API at `backboard.railway.com/graphql/v2` for anything
the CLI does not expose, which needs a `railwayapp/` User-Agent.

## Commits

Plain commit messages. No `Co-Authored-By` trailers.
