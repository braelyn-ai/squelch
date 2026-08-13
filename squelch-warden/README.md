# squelch-warden

The cluster-side tenant provisioner for hosted Passband. One single-node k3s
box, one warden pod, one squelchd pod per tenant, each with its own volume, its
own age identity, its own NetworkPolicy, and its own subdomain.

The control plane (`squelch-control`, on Railway) is the only caller. It makes
two calls to create a tenant, and three more to run one.

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
CIDR to 8848. A kubelet readiness probe originates at the NODE's address and
matches no pod and no namespace, so a CNI that does not exempt host-originated
traffic drops every probe and every provision times out on a healthy pod. It is
opt-in because whether it is needed depends on the CNI, and it admits
node-originated traffic only: another tenant's pod still arrives from a pod IP
and is still refused. Check it with a canary before you have tenants; SETUP.md
step 9 has the procedure and the tenant-to-tenant denial test to run beside it.

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
| `SQUELCH_WARDEN_NODE_CIDR` | unset | Lets the node reach 8848, when the CNI drops kubelet probes. |
| `SQUELCH_WARDEN_RUN_AS_UID` | `10001` | uid/gid/fsGroup for tenant pods. Never 0. |
| `SQUELCH_WARDEN_READY_TIMEOUT_SECS` | `180` | How long provisioning waits for a pod. 10 to 900. |
| `SQUELCH_WARDEN_PENDING_TTL_SECS` | `86400` | How long an abandoned signup keeps its label. 600 minimum. |
| `SQUELCH_WARDEN_TRUSTED_PROXY_HOPS` | `0` | Proxies in front of this listener, for rate-limit keying. Max 8. |
| `SQUELCH_WARDEN_LOG` | `info` | `tracing` filter. |

Every one of these is validated at boot, and a bad value is a refusal to start
with a sentence rather than a kube error on the first signup hours later. The
table of refusals is a test (`config.rs`).

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

On demand and per label, not a controller, deliberately: a loop that re-applies
to every tenant is a loop that can take every mailbox down on one bad render,
whereas a route an operator drives is run against one tenant, verified, and then
walked across the list. `squelch-control drift` (fleet-wide, exits 1 on drift)
and `squelch-control reconcile <label>` are the other end of both.

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
refuses to apply while the old pod still holds the volume, and refuses a tenant
with no workload at all, that the sweep collects abandoned pending tenants and
nothing else, every 4xx path, that DELETE keeps the volume and both Secrets, a 401 for
every way of getting the bearer wrong, that a cluster error never reaches a log
line verbatim, the boot-refusal table for every environment variable, and the
pairing parser against a captured copy of the daemon's real output.
