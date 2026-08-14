# squelch-warden

The cluster-side tenant provisioner for hosted Passband. One single-node k3s
box, one warden pod, one squelchd pod per tenant, each with its own volume, its
own age identity, its own NetworkPolicy, and its own subdomain.

The control plane (`squelch-control`, on Railway) is the only caller of the API.
It makes two calls to create a tenant, and three more to run one.

One other thing runs this binary, and it is not a caller: `squelch-warden roll`,
a CronJob on the same image that converges the fleet onto today's render from
inside the cluster. See "The roller", below.

Runbook for a fresh box: [`deploy/hosted/SETUP.md`](../deploy/hosted/SETUP.md).
Architecture and the decisions behind it: [`docs/HOSTED.md`](../docs/HOSTED.md).

## What it is trusted with

The tenant namespace, and nothing else. Its ServiceAccount carries a Role bound
into `tenants`: no ClusterRole, no cluster-admin, no access to its own
namespace, deliberately no `delete` on PersistentVolumeClaims, and every verb it
does hold is one `src/cluster.rs` calls. Its bearer token is that namespace, and
that namespace is the blast radius, bounded further by Pod Security Admission at
`restricted` on the namespace itself: holding the token lets you create a
workload there, and PSA decides what kind.

It holds `delete` on Secrets for exactly one job, the pending sweep, and the
narrowing is in the code rather than in RBAC: only an identity Secret, only
while that tenant is `pending`, only past the TTL, never a credential Secret.

**Not** anyone's mail. The warden mints each tenant's age identity in memory,
writes it straight into that tenant's Secret, and forgets it. The control plane
learns only the public recipient. So the warden cannot read a credential a
second after it stores one, and the control plane could never read one at all.

That is enforced, not just intended: a credential body that is not ASCII-armored
age is refused with a 422 before it reaches the API server. If the control plane
ever handed over a plaintext refresh token, this is the last place on the path
that can notice, and it does.

The crate does not depend on `squelch-core`. No store, no mail parser, no OAuth
client.

## The hard rule

**Nothing in this crate renders YAML.** Every tenant object is a typed
`k8s-openapi` struct built in `src/objects.rs`, and a tenant label reaches the
cluster only as a `TenantName`, which cannot be constructed without passing the
label validator. There is a test that greps this crate's own source for the
things that would break that rule.

The static manifests that install the warden itself are YAML, in
`deploy/hosted/`, where nothing is per-tenant.

## Wire

Every `/v1` route takes `Authorization: Bearer $SQUELCH_WARDEN_TOKEN`, compared
in constant time. Anything else is a bare 401 with no body, counted (a count,
never the value) and logged at warn. In front of the gate is a per-client-IP
token bucket at 120 requests a minute, so a flood is refused with a 429 before
it costs a compare.

| Route | Success | Failures |
|---|---|---|
| `POST /v1/tenants` | `201 { recipient }` | `409` label taken, `422` invalid label / address, `400` malformed JSON |
| `PUT /v1/tenants/{label}/credentials` | `200 { pair_code, pair_url, deep_link }` | `404` unknown label, `409` already serving, `422` unarmored ciphertext, `500` machine reason |
| `GET /v1/tenants/{label}` | `200 { status }`: `pending` / `active` / `failed` / `stopped` | `404` |
| `GET /v1/tenants/{label}/drift` | `200 { status, deployment_present, foreign, changes }` | `404`, `422` invalid label, `500` machine reason |
| `POST /v1/tenants/{label}/reconcile` | `200 { deployment, status }`: `converged` / `recreated` / `created` | `404`, `409 not_reconcilable`, `500` machine reason |
| `POST /v1/tenants/{label}/pair` | `200 { pair_code, pair_url, deep_link }` | `404`, `500 no_ready_pod` |
| `DELETE /v1/tenants/{label}` | `204` (workload gone, **data kept**) | - |
| `GET /healthz` | `200 ok` (no token: it is a probe and says nothing) | - |

```json
POST /v1/tenants        { "label": "alice", "account_email": "alice@example.com" }
                     -> { "recipient": "age1..." }

PUT  /v1/tenants/alice/credentials
                        { "cred_read_ciphertext": "-----BEGIN AGE ENCRYPTED FILE-----\n...\n" }
                     -> { "pair_code": "ABCD-1234", "pair_url": "...", "deep_link": "passband://pair?..." }
```

### Why two calls

The control plane cannot seal anything until it knows this tenant's recipient,
and the recipient does not exist until the warden mints it. So phase one mints
and returns the public half; phase two hands back the sealed blob and the tenant
comes up.

A signup that dies between them leaves a `pending` tenant, and phase one is
idempotent for exactly that case: **re-posting the same label with the same
address returns the same recipient**, so a retry never orphans a key. A
different address on a pending label is a 409. A label that is already serving
is a 409 for both calls. A pending tenant nobody comes back for is collected by
the sweep once it is older than `SQUELCH_WARDEN_PENDING_TTL_SECS`, which deletes
its identity Secret and frees the subdomain; recovery is signing up again.

Phase two is retryable too: "already provisioned" means a ready replica exists,
not merely that some objects do, so a `PUT` that died waiting for a pod can be
repeated and will converge rather than bounce off a 409 forever.

**And it is the re-consent path.** A daemon reads a COPY of the sealed blob on
its own volume (a Secret mount is read-only, and the file is rewritten on every
token refresh), so a new Secret alone would never reach it. Two things make it
land: the ciphertext's SHA-256 rides in an annotation on the pod template, so a
new blob is a new pod spec and the Deployment rolls; and the init container
keeps a marker of what it last installed and re-copies exactly when the mounted
Secret differs from it. A daemon-refreshed credential survives restarts, a
genuinely new one installs. Since an `active` tenant is a 409, rotating a
running tenant's credential is `DELETE` (which keeps the mail) and then `PUT`.

`DELETE` is idempotent: an unknown label is `204`, because the control plane
calls it on its own unwind paths and should not have to special-case a 404.

A `500` body is a machine reason and nothing else (`not_ready`, `pair_failed`,
`pair_output_unparsed`, `workload_failed`, `cluster_unavailable`, ...). No
namespace, no API error, no mailbox address ever crosses the wire. The detail is
in this pod's log.

## What a tenant is

Seven objects in namespace `tenants`, all named from the label:

| Object | Name | Survives DELETE |
|---|---|---|
| Secret (age identity, recipient, address) | `<label>-identity` | yes |
| Secret (sealed credentials) | `<label>-credential` | yes |
| PersistentVolumeClaim | `<label>-data` | yes |
| NetworkPolicy | `<label>` | no |
| Service | `<label>` | no |
| Deployment | `<label>` | no |
| Ingress | `<label>` | no |

Phase two applies them in that order bottom-up from the credential: volume
first, policy before the pod it polices, Ingress last so a hostname only starts
answering once something is behind it. `DELETE` removes them Ingress first and
NetworkPolicy last, so a terminating pod stays policed for the whole of its
termination.

### The pod

Non-root at uid 10001, `allowPrivilegeEscalation: false`, all capabilities
dropped, `RuntimeDefault` seccomp, a read-only root filesystem with exactly two
writable mounts (`/data` and `/tmp`), no ServiceAccount token, no service-link
environment, and `hostUsers: false` unless the operator turned user namespaces
off. That is the `restricted` Pod Security Standard, which the namespace also
enforces at admission.

Both containers carry CPU, memory and ephemeral-storage requests and limits, and
`/tmp` carries a `sizeLimit`: nothing in the pod is unbounded, because an
unbounded tenant is every other tenant's problem. The daemon's numbers come from
config; the init container's are fixed and tiny. The namespace-wide ceiling is
`deploy/hosted/70-tenant-limits.yaml`.

Its environment carries the tenant's mailbox address, the Google OAuth client
and, when a gateway is configured, the tenant's virtual LLM key — each by
`secretKeyRef` only, never inline: an address in a pod spec is an address in
`kubectl get deploy -o yaml`, and so is a client secret.

The image's entrypoint is bypassed (`command: /usr/local/bin/squelchd`,
`args: serve`) because `docker-entrypoint.sh` starts as root to chown the volume
and drops with `setpriv`, which cannot work under `runAsNonRoot`. `fsGroup` does
that chown at the kubelet instead.

An init container seeds two files onto the tenant's volume before the daemon
starts. The credentials one is not optional: `FileCredentialStore` rewrites that
file on every token refresh, and a Secret volume is read-only, so a daemon
pointed straight at the mount would sync happily for an hour and then fail
forever. It copies when, and only when, the mounted Secret hashes to something
other than the marker it left last time, which is what keeps a refreshed
credential across restarts and still lets a re-consent install. The second file
is the embedding-weights cache, when a shared model volume is configured.

### The OAuth client

Tenant daemons refresh with the control plane's confidential **web** client,
from a Secret shared by every tenant in the namespace
(`SQUELCH_WARDEN_OAUTH_SECRET_NAME`, keys `client_id` / `client_secret`). Not a
choice: a Google refresh token only works for the client that minted it, and
`squelchd serve` exits at boot without an OAuth client. It is custody hosted
already admits to, and on its own it opens no mailbox. See
`deploy/hosted/SETUP.md`, "The Google OAuth client".

### The LLM key

Stage-2 triage needs one, and hosted tenants each get their own: a per-tenant
virtual key for the LLM gateway, minted by the control plane and installed as
that tenant's `<label>-llm` Secret (key `api-key`), injected as
`SQUELCH_STAGE2_API_KEY` when `SQUELCH_WARDEN_LLM_BASE_URL` is set. The
reference is **optional**: an unminted tenant resolves no key at all and runs
heuristic-only triage, rather than wedging in `CreateContainerConfigError`. No
tenant ever holds a real provider key; the gateway holds the one real key and
meters each virtual key against its budget.

Both claims hold for every pod this warden renders — and only those. A tenant
provisioned before the shared-key bridge was removed still carries the legacy
`ANTHROPIC_API_KEY` env from its old spec, and its Stage-2 calls are failing
with 401s rather than idling, until `squelch-control llm mint` re-applies its
Deployment. The migration steps live in `deploy/hosted/PRODUCTION.md`,
"History".

### The console's Google link

`SQUELCH_WARDEN_CONSOLE_SSO_URL`, an origin, injected as `SQUELCH_CONSOLE_SSO_URL`.
Google forbids wildcard redirect URIs, so `<label>.<base domain>` cannot run
OAuth itself; the daemon's `/console` login page instead links to the control
plane, which authenticates the mailbox, mints a pairing code through this warden
(`POST /v1/tenants/{label}/pair`), and sends the browser back with it. Unset, and
the button does not render — which is the self-host posture, and what a hosted
deploy gets too if nobody configures it. Nothing is trusted on the way back: what
returns is a pairing code the tenant's own store adjudicates.

### The agent door

Each tenant's Ingress declares exactly three path prefixes: `/client`,
`/console` and `/t`. `/mcp` matches no rule, so the ingress controller answers
404 for it. An allowlist rather than a deny rule, because a deny rule has to
name every spelling of the thing it refuses and fails open when it misses one.
The list is `HUMAN_DOOR_PREFIXES` in `src/objects.rs`, with a test on it.

### The network

Ingress: only the ingress controller's pods, only on 8848. Egress: CoreDNS, and
TCP 443 to `0.0.0.0/0` minus every RFC 1918 range, the CGNAT range and
link-local. On a default k3s that subtraction removes the pod CIDR, the service
CIDR, the in-cluster API server, the warden, and every cloud metadata endpoint
in one stroke.

A second ingress rule carries the metrics allowance: the `monitoring`
namespace's `app.kubernetes.io/name=prometheus-agent` pod may reach 9464, where
the daemon serves Prometheus text (`SQUELCH_METRICS_BIND`, set on every tenant).
That listener is unauthenticated, so this rule is the whole control: one
namespace and one pod label on the same peer, one port, and not 8848. The port
is on the pod and on nothing that publishes it, so no `HUMAN_DOOR_PREFIXES`
mistake can route to it.

One optional third ingress rule: `SQUELCH_WARDEN_NODE_CIDR`, which allows that
CIDR to 8848 and 9464 — the two ports a kubelet probe can land on, both of them
whichever probe is currently rendered (see below), because a NetworkPolicy is the
one object no roll converges and a hole that tracked the probe would have to be
re-applied by hand, per tenant, in the same edit that moved it. A kubelet
readiness probe originates at the NODE's address and matches no pod and no
namespace, so a CNI that does not exempt host-originated traffic drops every
probe and every provision times out on a healthy pod. It is opt-in because
whether it is needed depends on the CNI, and it admits node-originated traffic
only: another tenant's pod still arrives from a pod IP and is still refused.
Check it with a canary before you have tenants; SETUP.md step 9 has the
procedure and the tenant-to-tenant denial test to run beside it.

### Readiness, and why the honest probe is opt-in

A tenant's readiness probe is a bare TCP accept on 8848 by default. That answers
the wrong question, and the gap is real: `squelchd` binds its listeners BEFORE it
builds its embedder, deliberately, so a first-run model download cannot leave the
doors unreachable. A probe that only opens a socket therefore calls a tenant
Ready seconds into a startup that has not happened — and goes on calling one
Ready whose sync engine died on a credential Google stopped accepting.

`SQUELCH_WARDEN_HTTP_READINESS=on` switches it to an HTTP GET of `/healthz` on
9464, the metrics port. The daemon answers `200 ok` there only once its sync
engine is running and its embedder init has settled, `503` until then and `503`
again if startup comes apart afterwards. Two states, one word, no counts and no
account information: it is on the unauthenticated listener, so everything it says
is said to anything that can reach that port. It is on that listener rather than
on a door for the same reason — the doors serve nothing unauthenticated, and a
probe holding a bearer token would be a credential in a pod spec.

**Default off is load bearing.** `/healthz` exists only on a daemon image new
enough to serve it. A tenant still on an older one fails an HTTP probe on every
period, never reports Ready, is taken out of its own Service for it, and halts
the roller when it comes round. The order is:

1. Ship the daemon that serves the route (`SQUELCH_WARDEN_IMAGE`).
2. Let the roller converge the whole fleet onto it, and confirm it did.
3. Then set `SQUELCH_WARDEN_HTTP_READINESS=on` and converge again.

`deploy/hosted/PRODUCTION.md`, "Rolling the daemon image", is the operator's end
of that sequence.

Behind both shapes sits `minReadySeconds` (`SQUELCH_WARDEN_MIN_READY_SECS`, 30s):
the Deployment controller does not count a replica Available until it has stayed
Ready that long, and `rollout_complete` waits on `availableReplicas` rather than
`readyReplicas`. Readiness is a snapshot and a daemon that comes up and dies
passes through it; Available is the count that waits. It costs half a minute per
tenant on a roll and it is defence in depth behind the roller's own pacing, so it
holds even for a tenant reconciled by hand.

## Environment

| Variable | Default | Notes |
|---|---|---|
| `SQUELCH_WARDEN_TOKEN` | **required** | 32+ characters. Refuses to start below that; never logged. |
| `SQUELCH_WARDEN_BASE_DOMAIN` | **required** | `passband.email`. Tenants are subdomains of it. |
| `SQUELCH_WARDEN_IMAGE` | **required** | The squelchd image tenants run. Must carry a tag or a digest. |
| `SQUELCH_WARDEN_BIND` | `0.0.0.0:8852` | It is a pod; loopback would make the Service answer nothing. |
| `SQUELCH_WARDEN_TENANT_NAMESPACE` | `tenants` | The only namespace this warden may touch. |
| `SQUELCH_WARDEN_INGRESS_CLASS` | `traefik` | `ingressClassName` on every tenant Ingress. |
| `SQUELCH_WARDEN_INGRESS_NAMESPACE` | `kube-system` | Where the ingress controller runs, for the NetworkPolicy. |
| `SQUELCH_WARDEN_INGRESS_POD_LABEL` | `app.kubernetes.io/name=traefik` | Which pods there may reach a tenant. |
| `SQUELCH_WARDEN_TLS_SECRET` | `passband-wildcard-tls` | The wildcard certificate, in the tenant namespace. |
| `SQUELCH_WARDEN_OAUTH_SECRET_NAME` | `google-oauth-client` | Secret holding the web client tenant daemons refresh with. |
| `SQUELCH_WARDEN_CONSOLE_SSO_URL` | unset | Control plane origin, passed on as `SQUELCH_CONSOLE_SSO_URL`. Origin only. |
| `SQUELCH_WARDEN_STORAGE_CLASS` | `local-path` | k3s's built-in provisioner. |
| `SQUELCH_WARDEN_STORAGE_SIZE` | `10Gi` | Per-tenant volume. |
| `SQUELCH_WARDEN_CPU_REQUEST` | `100m` | Tenant daemon container. |
| `SQUELCH_WARDEN_CPU_LIMIT` | `1000m` | Tenant daemon container. |
| `SQUELCH_WARDEN_MEMORY_REQUEST` | `256Mi` | Tenant daemon container. |
| `SQUELCH_WARDEN_MEMORY_LIMIT` | `1Gi` | Tenant daemon container. Past it, OOM-killed and restarted. |
| `SQUELCH_WARDEN_EPHEMERAL_REQUEST` | `256Mi` | `/tmp` plus logs. The mailbox is a PV and does not count. |
| `SQUELCH_WARDEN_EPHEMERAL_LIMIT` | `1Gi` | What stops a tenant filling the node's root filesystem. |
| `SQUELCH_WARDEN_TMP_SIZE` | `512Mi` | `sizeLimit` on the pod's `/tmp` emptyDir. |
| `SQUELCH_WARDEN_USER_NAMESPACES` | `on` | `hostUsers: false`. Turn off only if the cluster cannot do it. |
| `SQUELCH_WARDEN_MODEL_PVC` | unset | Shared pre-seeded embedding weights; see SETUP.md step 10. |
| `SQUELCH_WARDEN_IMAGE_PULL_SECRET` | unset | For a private squelchd image. |
| `SQUELCH_WARDEN_NODE_CIDR` | unset | Lets the node reach 8848 and 9464, when the CNI drops kubelet probes. |
| `SQUELCH_WARDEN_HTTP_READINESS` | `off` | Probe `/healthz` on 9464 instead of accepting on 8848. Turn on only once the whole fleet runs a daemon that serves it. |
| `SQUELCH_WARDEN_MIN_READY_SECS` | `30` | `minReadySeconds`: how long a pod must stay Ready to count Available. 0 to 300, and below the ready timeout. |
| `SQUELCH_WARDEN_RUN_AS_UID` | `10001` | uid/gid/fsGroup for tenant pods. Never 0. |
| `SQUELCH_WARDEN_READY_TIMEOUT_SECS` | `180` | How long provisioning waits for a pod. 10 to 900. |
| `SQUELCH_WARDEN_PENDING_TTL_SECS` | `86400` | How long an abandoned signup keeps its label. 600 minimum. |
| `SQUELCH_WARDEN_TRUSTED_PROXY_HOPS` | `0` | Proxies in front of this listener, for rate-limit keying. Max 8. |
| `SQUELCH_WARDEN_LOG` | `info` | `tracing` filter. |

Every one of these is validated at boot, and a bad value is a refusal to start
with a sentence rather than a kube error on the first signup hours later. The
table of refusals is a test (`config.rs`).

On the hosted install they are one ConfigMap, `deploy/hosted/15-warden-config.yaml`,
which both the serving pod and the roller pod take through `envFrom`. Two
processes render tenants from these values, so one object is what stops them
rendering different ones; the bearer token (a Secret) and each pod's own
container image are the only things outside it.

## State

There is none, and one timer. No database, no state file, no port allocator, no
unwind; the timer is the pending sweep, which reads the cluster and writes
nothing but a delete. The
cluster is the record and kube's control loop is the reconciler; a tenant's
status is derived from which objects exist and whether the Deployment has a
ready replica. Two signups racing on one label are settled by the API server:
the loser of `create` on the identity Secret gets the same 409 a serialized pair
would have produced.

Every apply is a server-side apply, so a retried phase two converges instead of
duplicating. That is why there is no unwind code: a best-effort teardown running
on a cluster that is already misbehaving is worse than leaving objects the next
attempt overwrites.

### What reconciles, and only when asked

Kube's control loop keeps each tenant's pod matching the objects the warden
wrote. **Nothing keeps those objects matching the warden's current code on its
own.** A tenant's Ingress, NetworkPolicy, Service and Deployment are written
during phase two and are not revisited until somebody asks.

So a change to the SHAPE of a tenant reaches new tenants by itself and existing
ones never: a path added to `HUMAN_DOOR_PREFIXES`, a new environment variable in
the pod, a changed NetworkPolicy peer. Deploying a new warden is not a migration.
What it is instead is two routes an operator drives:

- **`GET /v1/tenants/{label}/drift`** — what is wrong with this tenant.
  Read-only; the only write it makes is a dry run.
- **`POST /v1/tenants/{label}/reconcile`** — put it back on today's render.

Per label, and driven: nothing watches these objects and nothing re-applies to a
tenant because a second went by. `squelch-control drift` (fleet-wide, exits 1 on
drift) and `squelch-control reconcile <label>` are the other end of both routes.

The fleet gets walked too, by a timer rather than by a route — `squelch-warden
roll`, below. It is a caller of `reconcile` and not a controller: it walks the
fleet once, in order, and exits. It converges what a drift report can see, which
is the Deployment; the Ingress, the NetworkPolicy, the Service and the PVC reach
an existing tenant only through a `reconcile` somebody asks for.

#### Two detectors, because each kind of drift is invisible to the other

A drift report answers two independent questions:

1. **Who else owns something here?** `metadata.managedFields` is the API
   server's ownership ledger. `drift::foreign_managers` walks every entry that
   is not the warden's (and not the controller's `status` bookkeeping) into
   dotted paths, and any surviving path is the finding.
2. **What would an apply change?** The warden renders this tenant's Deployment
   from today's code, sends it as a `dryRun=All` server-side apply, and diffs
   the merged object the API server hands back against the live spec.

Detector 1 is needed because SSA owns FIELDS, not objects: a field the warden
does not declare belongs to whoever wrote it, so the warden's applies converge
around it forever and detector 2 never mentions it — it is identical on both
sides of that diff. One `kubectl set env` is otherwise invisible to this service
for the rest of the tenant's life.

Detector 2 is needed because ownership says nothing about VALUES. A tenant
provisioned two releases ago is entirely warden-owned, with a spotless ledger,
running an old image, an old env block and old resource bounds.

Exactly one path is dropped from a report,
`metadata.annotations.deployment.kubernetes.io/revision`: the deployment
controller writes it on every rollout, so reporting it would mark every tenant
permanently drifted and make the report worth nothing.

#### Delete-and-recreate, and the wait in the middle

A reconcile re-applies the PVC, NetworkPolicy, Service, Deployment and Ingress
in provision order and does not answer until a pod is Ready. When foreign
managers own part of the Deployment it deletes the Deployment first and applies
a fresh one, whose ownership ledger starts empty and carries exactly what the
warden declares. There is no forced apply that takes a foreign field back:
`--force-conflicts` settles a conflict over a field the applier declares and
never removes one it does not. So the answer says which happened — `converged`
(applied in place), `recreated` (a purge, and a rolled pod), `created` (a
`DELETE` landed while it was working).

Between the delete and the apply it waits for the old pod to be GONE, not merely
not-Ready. The data volume is `ReadWriteOnce` and the daemon is one SQLite file;
inside one Deployment the `Recreate` strategy guarantees no overlap, but across a
delete and a re-create nothing holds that promise, so `Cluster::pods_gone` holds
it here. A timeout is a refusal (`pods_not_gone`), never a second writer.

While that window is open the tenant has no Deployment, so a reconcile that dies
inside it leaves a tenant reading `stopped` — the same word a cancelled account
reads. Run the reconcile again once the pod lets go and it finishes the job; the
volume, the identity and the credential never moved.

`active` and `failed` both proceed. Failed is precisely the incident state, a
pod wedged on a foreign secret reference with no ready replica, and refusing it
would make the route useless in the case it exists for. `pending` is
`409 not_reconcilable`: it has never had a workload, and bringing one up is a
signup to finish, not a shape to converge.

`stopped` depends on why it stopped, and the status word cannot say. `DELETE`
takes the Ingress, the Deployment, the Service and the NetworkPolicy down
together, while a reconcile applies the Service before it touches the
Deployment — so a surviving Service means the workload is still routed and only
the Deployment is missing. That is a job to finish, and it proceeds. No Service
means somebody cancelled this account, and it stays `409`: starting a cancelled
mailbox back up would be a resurrection nobody asked for. The check is the
Service and not the Ingress deliberately, because the Ingress is the object an
operator was most likely to have applied by hand during the era this route
replaces, and a hand-applied Ingress must not read as consent to restart.

Secrets are never rewritten. A reconcile converges SHAPE; the sealed credential
and the LLM key are read back only to re-derive the two SHA-256 annotations on
the pod template, so the render is the one that tenant is entitled to rather
than a new one, and a re-render on its own does not roll the pod.

#### The roller: the whole fleet, one tenant at a time

```sh
squelch-warden roll             # converge every tenant onto today's render
squelch-warden roll --dry-run   # every read, no writes: what a roll would move
```

The same binary, a different job, run by the CronJob in
`deploy/hosted/90-warden-roller.yaml` on the warden's image, ServiceAccount and
environment — the environment literally, one ConfigMap
(`deploy/hosted/15-warden-config.yaml`) behind both pods' `envFrom`, because two
copies of these values are two different renders of the same tenant taking turns
rewriting it. Not a route and not a bearer-authed call: a converging pass over
every tenant is the most powerful thing this service can do, so it stays inside
the cluster, where no credential and no CI job can reach it.

**What it converges.** Every tenant the CLUSTER holds — the identity Secrets
carrying `MANAGED_SELECTOR`, sorted, so two runs are comparable and a tenant with
no row in the control plane's table is still seen. For each: the status, then a
drift report, then, only if the report has changes, `reconcile`. One tenant at a
time, and the next one is not touched until this one's ROLLOUT has finished
(`Cluster::rollout_complete`, not a ready pod — under `Recreate` the pod being
replaced stays Ready while it terminates, and a roller trusting that would march
through the fleet on false greens). Finished means AVAILABLE, not merely ready:
the replica has to have stayed up for `SQUELCH_WARDEN_MIN_READY_SECS`, so a
daemon that comes up and dies cannot buy the run a green. A tenant blips for one
pod restart; the fleet is never down.

**The Deployment, and only the Deployment.** What decides whether a tenant is
rolled is the drift report, and a drift report renders and diffs that tenant's
Deployment alone. So a change that lands anywhere else — a tenant's Service,
Ingress, NetworkPolicy or PVC — is invisible to this pass and always will be. See
"What the drift report cannot see", below, where it is the first limit listed
and the one most likely to be mistaken for a converged fleet.

**What it refuses to touch, and this is the important half.** A Deployment
another field manager owns fields on is SKIPPED and never repaired. `reconcile`
purges a foreign field the only way SSA allows, by deleting the Deployment and
applying a fresh one, which is a defensible call for an operator reading one
drift report and an indefensible one for a timer walking a fleet: it takes a live
mailbox down to remove a field a person put there on purpose. Foreign drift is a
page for a human. `pending` and `stopped` tenants are skipped for the reason
`reconcile` refuses them — a signup to finish and an account to reopen are not
shapes to converge — which leaves `active` and `failed`, and `failed` is
deliberate, because a tenant a previous render broke is the one a new render is
most likely to fix.

**Halting is the safety property.** The first tenant that does not converge ends
the run: it is named in the summary, and every tenant after it in fleet order is
left exactly as it was. A render that cannot come up therefore costs exactly one
tenant, which is what makes running this unattended defensible at all. A read
that fails halts it too — a tenant this warden could not even inspect is not one
it may step past, because the next tenant would be rolled on the strength of a
cluster that has just stopped answering.

The exit code is the whole interface for whatever scheduled it, and anything
other than `0` marks the Job failed on purpose:

| Exit | The run | What it wants |
|---|---|---|
| `0` | The fleet is on today's render and serving it. A run with nothing to do is this, and so is a `--dry-run` over a fleet that needs nothing. | Nothing. |
| `1` | Not converged, in any of its five forms. | Read the log for the named label, and suspend the CronJob while you work: everything after that tenant in label order was not touched. **Halted** — the tenant did not come back; look at that pod. **Casualty** (`HALTED before applying anything`) — the tenant already carries today's render and is not serving it, so nothing was applied anywhere; the render is the suspect, not the tenant. **DOWN** — no workload behind a live Service, an unfinished reconcile; `squelch-control reconcile <label>` finishes it. **Never started** — a refused config value or an API server it could not reach; the log line is the sentence. **`--dry-run` found work** — this is the flag doing its job; read what it would roll. |
| `2` | Converged, and at least one tenant was left alone because another field manager owns part of its Deployment. | `squelch-control drift <label>` to see who owns what, then `squelch-control reconcile <label>` when you are ready for that mailbox to be down for a pod cycle. Until then every run reports it again. |
| `64` | The argument list was none of the three this binary accepts. | Fix `args:` on the CronJob. Deliberately outside the 0–2 range: a mistyped argument is not a verdict on the fleet. |

A run that halts on the SAME label every tick is a render the cluster refuses
rather than a flaky tenant — the apply was rejected, so nothing was written, so
that tenant is still drifted and first in the queue again fifteen minutes later.
`deploy/hosted/90-warden-roller.yaml` has the signature and the fix, and
`deploy/hosted/PRODUCTION.md`, "Rolling the daemon image", is the operator's end
of all of it.

### What the drift report cannot see

Worth knowing before the answer "clean" is trusted too far.

**It sees a tenant's DEPLOYMENT and nothing else.** `Warden::drift` renders that
one object, dry-run applies it, and diffs the result; a tenant's Service,
Ingress, NetworkPolicy and PVC are never rendered and never compared. So none of
them can ever appear as drift, and none of them can trigger a roll. Delete a
tenant's Ingress out of band and both the report and the roll say that tenant is
already current, with the mailbox unreachable from the internet.

The consequence for configuration is the one to hold on to: a change to
`SQUELCH_WARDEN_TLS_SECRET`, `SQUELCH_WARDEN_NODE_CIDR`, the three
`SQUELCH_WARDEN_INGRESS_*` values, `SQUELCH_WARDEN_STORAGE_*` or
`HUMAN_DOOR_PREFIXES` reaches **new signups only,
forever**. The roller will not carry it, and will not mention it. The manual
answer is `squelch-control reconcile <label>` per tenant, which re-applies all
five objects in provision order; a tenant being rolled for some Deployment-visible
reason picks the rest up as a side effect of that, which is luck rather than
coverage.

**`reconcile`'s anti-resurrection guard has a narrow race, and the roller enters
it once per active tenant per tick.** A tenant with no Deployment is either a
cancelled account or a reconcile that died mid-repair, and the two are told apart
by whether the Service is still standing (`Warden::interrupted`) — `DELETE`
removes the Service before the Deployment, so a surviving Service means nobody
cancelled this account. The check is sound at the top of the route, where nothing
has been written yet. The second one is not: `reconcile` re-applies the Service
before it reads the Deployment, so a `DELETE` that lands in the window between
those two writes has its Service put back **by this reconcile**, and the
deployment-gone branch then reads that Service as consent and rebuilds a mailbox
somebody just cancelled. The window is a few API calls wide and needs a
cancellation landing inside it. What the roller changes is the frequency: the
window opens once per active tenant per run, every 15 minutes, rather than when a
person types a command. Cancelling an account while a roll is walking the fleet is
therefore worth doing deliberately — suspend the CronJob, or confirm the tenant
is gone afterwards.

**`status: active` in a reconcile's answer is weaker than it sounds.** It means
a pod matching the tenant's selector reported Ready, not that the pod running
the new spec did. On the converged path the old pod is often still Ready while
`Recreate` is terminating it, so a reconcile onto a render that cannot start —
an image tag GHCR does not hold is the easy way to do this — can answer
`200 converged` and then sit in `ImagePullBackOff`. Watch the pod after a
reconcile that changed the image or the env; do not take the answer for it.

**Duplicate names in a keyed list hide behind the first one.** `diff_spec`
aligns containers, env vars, volumes and ports by `name` and takes the first
match, so a second `env` entry with a name already present is invisible to the
diff. PodSpec validation permits it (last one wins at runtime) and a hand edit
is the likeliest way to get one. The ledger still names the manager that wrote
it, so the tenant is not silently clean — but the value is not shown.

**`squelch-control drift` with no label only knows tenants the control store
knows.** It enumerates the control plane's rows, and a tenant provisioned into
the cluster without one — which `squelch-control`'s own signup path can produce
and logs as `PROVISIONED BUT NOT RECORDED` — is invisible to that command
forever. Per label it works fine, and the roller does not share the blind spot:
it enumerates the cluster by `MANAGED_SELECTOR`, the way the pending sweep
lists, so the tenant most likely to have been finished by hand is the one it
still sees.

**That walk is also one request per tenant against a 120/minute bucket** shared
by everything reaching the warden through that ingress. Somewhere above a
hundred tenants a full sweep will start meeting its own rate limit, and a 429 is
reported as "could not be checked" rather than as drift. The roller is a library
call in the cluster and meets no limiter at all.

## Logging

Counts, statuses, and the tenant label. Never a mailbox address, never a
credential, never an identity, and never the output of the pairing exec:
`squelchd pair` prints a live pairing code, so the rule is blanket rather than
per-call.

Never an API error verbatim either. A kube error carries the API server's own
message, and the API server quotes the offending request back in some of them,
which on this service is somebody's sealed credential. What a failed step logs
is `ClusterError::summary`: the kind of failure, the operation, and an HTTP
status.

## Tests

```sh
cargo test -p squelch-warden
```

Every Kubernetes call goes through the `Cluster` trait, so the suite needs no
cluster, no kubeconfig, and no network. It asserts the exact ordered list of
typed objects a two-phase provision applies, the full pod securityContext, that
no container and no volume is unbounded, the OAuth client arriving by
`secretKeyRef`, the NetworkPolicy peers and CIDR exceptions (with and without a
node CIDR), that the metrics port is admitted to one pod and published by
neither the Service nor the Ingress, the `/mcp` arrangement on the Ingress, the
pending-label idempotency, the 409 both ways round including a lost create race,
that a new ciphertext rolls the pod and a stopped tenant can be
re-credentialed, that a drift report names the manager behind a hand edit and
says nothing about the three owners every healthy Deployment has, that a
reconcile converges a clean tenant, delete-recreates one another manager owns,
refuses to apply while the old pod still holds the volume, refuses a tenant
with no workload at all, and answers only once the rollout is complete rather
than on the first Ready pod, that a fleet roll walks every tenant in the cluster
in sorted order and moves only the drifted ones, leaves the ones another manager
owns and the ones with no workload where they are, halts on the first tenant
whose rollout does not finish and names it, and writes nothing at all in a dry
run, that the sweep collects abandoned pending tenants and nothing else, every
4xx path, that DELETE keeps the volume and both Secrets, a 401 for every way of
getting the bearer wrong, that a cluster error never reaches a log line
verbatim, the boot-refusal table for every environment variable, the binary's
argument grammar and the exit code each roll outcome maps to, and the pairing
parser against a captured copy of the daemon's real output.
