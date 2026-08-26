# One embedding session per node

Status: designed 2026-08-26, **not built**; the build trigger is §9. The design
record for the change that makes hosted tenant count independent of the ONNX
Runtime session: one embedder per node, shared by every tenant, instead of one
per tenant pod. Written now and built later on purpose, because the Phase 1
daemon work (idle unload, heap trim) buys enough headroom that building today
would be premature, and these decisions are cheaper to argue about before there
is code.

## 1. Why

Every hosted tenant runs its own `squelchd` pod on `carrier` (Hetzner CPX21, 3
shared vCPU, 3.8 GB, 80 GB, no swap). Each embeds mail for semantic recall with
fastembed over ONNX Runtime, `bge-small-en-v1.5` at 384 dimensions, weights
pinned to `Xenova/bge-small-en-v1.5` fp32. Measured on the fleet:

| | Resident |
|---|---|
| Rust side of a tenant daemon (store, sync, both doors) | 25-40 MB |
| The same pod once it has embedded anything | 250-300 MB |

The ONNX session is 85-90% of a tenant pod. Roughly 126 MB of that is the fp32
weights; the rest is ORT's arena, activation buffers it allocates on the first
run and retains. PR #144 measured the per-run half directly, which is where the
batch numbers in §2 come from:

| | `max_length` 512 | `max_length` 256 |
|---|---|---|
| batch-8 pass | +324 MB | +123 MB |
| batch-1 pass | +44 MB | +13 MB |

The arithmetic, assuming about 1.2 GB of node overhead (k3s, Traefik, the
monitoring agents, litestream) and so about 2.6 GB for tenants. Treat the
overhead figure as an estimate; the ratios are what matter.

- **Today, about 7 tenants.** 2.6 GB over ~350 MB per pod. Past that we
  oversubscribe a box with no swap, which means the OOM killer picks a tenant.
- **After Phase 1 and Phase 2, about 20.** Unloading the session after ~10
  minutes idle takes an idle tenant to ~40 MB and leaves an active one at ~250
  MB. Phase 2 then makes the memory *request* honest, and an honest request has
  to cover a good fraction of the active case, so the scheduler admits about 20
  before `deploy/hosted/70-tenant-limits.yaml` (`requests.memory: 5Gi`) refuses
  the next. That refusal is the good failure: provisioning fails at the API
  server rather than into a box that cannot run what it accepted.
- **With one shared session, 50 or more.** A tenant pod with no ONNX session is
  40-50 MB including the HTTP client, against one 300-400 MB service on the node.
  `(2600 - 400) / 45` is about 48. Past that the binding constraint is no longer
  memory: it is the 200 GB storage quota, the 25-PVC cap, and 3 vCPU of Gmail
  poll loops.

The shape of the win is the point. Today the ONNX cost is `N x 300 MB`; after
this it is `1 x 300 MB` plus a few MB per tenant, so the ONNX line leaves the
capacity arithmetic entirely. Cross-tenant batching makes the CPU cheaper on the
same stroke: a batch-of-one from tenant A and a batch-of-one from tenant B are
two full forward passes today, and coalesced they are one.

## 2. Shape

### The unit: a new crate and its own image

`squelch-embed`, a new workspace member with one small binary, built by a new
`Dockerfile.embed`. The alternative was a `squelchd embed-serve` subcommand
reusing the daemon image, which keeps the image count at one. Three reasons it
loses:

1. **Release cadence.** The daemon image is what the fleet roller pins and walks
   tenants onto one per tick. Making the shared embedder a mode of that image
   ties every tenant's embedding availability to a rollout mechanism designed to
   move slowly and refuse to proceed past a casualty.
2. **Weights.** The service bakes the 126 MB of ONNX weights into its own image,
   which is what lets it need no model PVC and no egress at all (§4). Baking them
   into the daemon image would put 126 MB into every self-host pull for a feature
   self-host does not use, and `deploy/hosted/SETUP.md` §10 deliberately keeps
   the daemon image weight-free.
3. **What is in the process.** The binary every tenant's mail text flows through
   should have a one-sentence description. `squelch-embed` opens no store, mounts
   no credential, and has no Gmail or LLM client reachable from any route. It
   does link `squelch-core` for the `Embedder` trait, so squelch-core is in its
   dependency graph regardless: linking is not reachability, and the isolation
   that holds is in the pod spec, not the link graph.

The cost is one more image in CI and one more thing to roll.

### The objects

- **One Deployment**, `replicas: 1`, in a namespace of its own (`embed`). Not
  `tenants`, because `pods: "25"` in that namespace's ResourceQuota is the
  admission control on **tenant count** and a non-tenant pod eating a slot makes
  the quota mean something other than what its comment says. Not `warden`,
  because that namespace is deliberately unlabelled for Pod Security Admission
  (its pod carries an API token) and the embedder should run under `restricted`
  like a tenant does.
- **One ClusterIP Service**, port 8850, named `http`. The metrics port is absent
  from the Service, exactly as a tenant's is.
- **One NetworkPolicy on the service** (§4) and **one egress delta on every
  tenant's** (§5). No Ingress: nothing outside the cluster ever reaches it.

### The API

Two listeners, following the daemon's pattern: the work on 8850, and `/healthz`
plus `/metrics` on a second port (9464) the Service does not carry.

```
POST /embed
  { "texts": ["subject\n\nbody", "..."] }
  -> 200 { "dims": 384, "vectors": [[...384 floats...], [...]] }
```

`vectors` is positional: same length as `texts`, same order. `dims` is echoed on
every response so the daemon can re-check it against the vec0 width without a
second route.

| Status | When | What the daemon does |
|---|---|---|
| 400 | shape or bounds violated | logs, gives up on that text (a bug, not a transient) |
| 429 | queue full, with `Retry-After` | drops the batch; backfill recovers it (§3) |
| 503 | session not built yet | same as 429 |
| 5xx | anything else | same as 429 |

### Bounds, and which one is the memory bound

Four numbers, and only one is about memory:

- **`MAX_TEXTS_PER_CALL = 64`**, because the daemon's `backfill_batch` default is
  64 and a wire bound that forces the daemon to re-chunk is a bound that will get
  out of step with the daemon's config.
- **`MAX_CHARS_PER_TEXT = 8192`**, and a 1 MB body limit. A sanity bound on the
  wire, not a product decision: the daemon already truncates to `embed.max_chars`
  (1000 after PR #144) before it sends. If the two disagree the daemon's number
  is the real one; this exists so a malformed caller cannot post a megabyte.
- **`MAX_QUEUED_TEXTS = 256`**, the coalescer's inbound queue. Past it, 429.
- **`INFERENCE_BATCH = 8`. This is the memory bound**, and deliberately not the
  same number as `MAX_TEXTS_PER_CALL`: a 64-text request is split into eight ORT
  runs of eight. From §1, batch-8 at 256 tokens costs +123 MB of scratch;
  batch-64 at 512 tokens is +1.7 to +2.7 GB, which is the whole box. The
  coalescer's cap **is** the service's memory ceiling, so it is a constant with
  that sentence next to it, not a tunable.

Sequence length is fixed at the daemon's `max_tokens` (256 after PR #144),
because the two must produce vectors in the same space and `max_length` changes
what the model reads. If that number moves, it moves in both places in one commit
or existing vectors stop being comparable.

### Cross-tenant batching

Requests wait up to **10 ms** in a coalescing queue, then whatever has arrived
(capped at `INFERENCE_BATCH`) goes through as one ORT run. 10 ms is short enough
to be invisible at query time (a search already costs tens of ms of SQLite work)
and long enough to collect the overlapping arrivals batching exists for. A batch
fires when it is full or the window closes, whichever comes first.

Two tenants' texts sit in the same ORT batch. That is the design, and §4 is what
makes it acceptable: the batch is a transient positional array, results return by
index to the connection that supplied them, and nothing is retained after the run.

Pod resources: `requests: 320Mi / 200m`, `limits: 768Mi / 2000m`. The request
covers weights plus one batch of scratch; the limit leaves room for the arena's
high-water mark without letting a bug take the node.

## 3. The daemon side

A `RemoteEmbedder` in `squelch-core/src/embed/`, implementing the existing
`Embedder` trait. `embed` posts one text, `embed_batch` posts up to
`MAX_TEXTS_PER_CALL` per call and chunks past that, `dims` returns the configured
384 without a network call so `attach_embedder`'s width check against `VEC_DIMS`
stays synchronous and stays exactly as it is today.

`SQUELCH_EMBED_URL` selects it. **When it is set, `FastEmbedder::new` is never
called** and no ONNX session is ever built in the daemon process. That single
fact is the entire saving, so it lives in one place: `build_embedder` in
`squelchd.rs` branches on the URL and the two arms are mutually exclusive. There
is no fall-back-to-local, because a daemon that quietly builds a local session
when the service is unreachable is a daemon that quietly costs 300 MB, which is
the problem this document exists to solve.

Readiness keeps its contract. "Embedder settled" today means the background init
resolved, either way; with a URL configured it means the URL is set and
`/healthz` answered once. It does **not** mean the service is currently healthy,
for the same reason readiness does not mean Gmail is currently reachable: a
mailbox with a degraded search index is a working mailbox, and answering 503
would pull the pod out of its own Service and stop the fleet roller dead.

Failure semantics are today's local-failure semantics, unchanged:

- **Ingest** (`SyncEngine::embed_and_store`) already logs a redacted line and
  returns. The vector is recovered by `backfill_missing_vectors` on a later pass.
  Ingest never blocks on the embedder and must not start to.
- **Backfill** (`backfill_missing_vectors`) already logs and stops the pass on a
  batch failure, retrying next tick. A 429 is exactly that case.
- **Query** needs one real change, and it is the one thing here that is a code
  fix rather than a new path. `SqliteStore::hybrid_search` today does
  `embedder.embed(query_text)?` and propagates. With a local embedder that
  effectively never fails once built; with a remote one it fails on any network
  blip, and a user's search would 500 instead of degrading. `hybrid_search` must
  treat an embed **error** the way it already treats an absent embedder: no
  vector leg, keyword results, one redacted log line. `semantic_search` (the
  pure-vector route) keeps propagating, because there is nothing to degrade to.

## 4. Security

Be blunt about what this is. **`squelch-embed` is a process we operate that sees
the subject and body text of every hosted tenant's normal-sensitivity mail, in
flight, in one address space.** That widens the hosted trust story and belongs in
`docs/SECURITY.md`, not only here.

It is not new **in kind**. Hosted already runs tenant bodies through a gateway we
operate: Bifrost sees every message Stage 2 triages, and `docs/HOSTED.md` says so.
This is a second such component with a narrower job.

**The sealed-content invariant is unchanged, structurally.** The gate is in the
store's embed-at-write path (`sensitivity == Normal`, in `SyncEngine::ingest_one`
and `messages_missing_vectors`), not in the embedder. Sealed text is never handed
to an `Embedder` of any kind, so it never reaches the wire. Nothing here touches
that gate, and nothing here may: an embedder that filtered its own input would be
a second gate to keep in step with the first.

**What the service must not do**, each a testable claim:

- **Never log text.** Counts, byte lengths, statuses and durations only, the
  discipline `squelch-relay` states in its module doc.
- **Never persist text.** No disk writes past the read-only weights: read-only
  root filesystem, no volume but a sized `/tmp`.
- **Never cache keyed by text.** A content-keyed cache is a cross-tenant oracle:
  tenant B learns from hit latency that tenant A received a byte-identical
  message. So no cache, and no in-batch dedup either (the thing a coalescer
  naturally invites), for the same reason and because it saves nothing.
- **Never attach a tenant identifier to metrics.** `squelch-core/src/metrics.rs`
  is the standard: closed label sets resolved from enums, never from data. Here
  that is unlabelled totals plus a closed `outcome` label (`ok`, `rejected`,
  `queue_full`, `error`). A per-tenant counter would put the fleet's mail volume,
  tenant by tenant, on an unauthenticated scrape, and a per-tenant series is also
  how you would learn a tenant exists at all.

**Authentication: NetworkPolicy is the boundary, and add a shared token anyway.**

Reachability is already the strong control. A tenant's NetworkPolicy is
default-deny with two egress rules: CoreDNS, and `0.0.0.0/0:443` minus
`BLOCKED_EGRESS_CIDRS`, which excludes `10.0.0.0/8` and so every in-cluster pod
and Service. Today a tenant pod **cannot reach the embed service at all**; after
§5 it can reach exactly that selector on exactly that port. The service's own
NetworkPolicy allows ingress only from `namespaceSelector: tenants` plus the
`app.kubernetes.io/managed-by=squelch-warden` pod selector, and from the
monitoring agent on the metrics port. A peer is a namespace **and** a pod
selector, never either. Egress is denied outright: with the weights baked in, the
service has nothing to dial, so a compromised embedder cannot ship text anywhere.

A bearer token therefore buys nothing for confidentiality against the threat the
policy already covers. Add one regardless (`SQUELCH_EMBED_TOKEN`, one fleet-wide
value in a Secret, constant-time compare), for two narrow reasons: a CNI
misconfiguration or a policy that fails open is a real class of outage and the
token is the belt under it, and a 401 is a better answer than serving something
that arrived by accident, and a better metric. One shared value, not per-tenant,
because the service must not be able to distinguish tenants (see the metrics
rule) and a per-tenant credential would hand it exactly that ability.

**TLS: no. Plaintext HTTP on the pod network.** The traffic never leaves the
node. TLS means a certificate to mint, rotate and expire, on a path whose only
observer would already have the node's network namespace and could read the
SQLite files directly. Compare `SQUELCH_WARDEN_LLM_BASE_URL`, which the warden's
validator **requires** to be https, correctly: Bifrost is remote and a virtual
key crosses the public internet. This service is neither.
`SQUELCH_WARDEN_EMBED_URL`'s validator is the mirror image and should say so at
the check: `http://` is allowed **because** the destination is in-cluster, and a
URL with a public host is refused outright.

**What `docs/SECURITY.md` gains.** A subsection under §4, roughly: hosted mail
text reaches two processes we operate, the LLM gateway and the embedding service;
sealed mail reaches neither, and the enforcement point for the second is the same
store-side `sensitivity == Normal` gate as for the first, so the invariant has one
enforcement point and not two; the embedding service logs nothing, stores nothing,
caches nothing, cannot name a tenant, and cannot reach the network; self-host
reaches neither process.

## 5. Warden changes

The warden stays the only thing that touches Kubernetes, and every object stays a
typed `k8s-openapi` struct.

- **New objects in `objects.rs`**: `embed_deployment`, `embed_service`,
  `embed_network_policy`, built the same way the tenant ones are (the same
  `PodSecurityContext` and `SecurityContext`, `automount_service_account_token:
  false`, `enable_service_links: false`). They are **fleet objects, not tenant
  objects**: applied once at warden startup or by an explicit
  `squelch-warden embed apply`, not per tenant, and not touched by the roller's
  per-tenant drift pass.
- **NetworkPolicy delta on every tenant**: one more egress rule, to
  `namespaceSelector: embed` plus the service's pod selector, TCP 8850. Note the
  subtlety for whoever writes the verification step: the rule names the **pod
  selector**, not the ClusterIP, because kube-proxy DNATs the Service address
  before the policy is evaluated. A rule written against `10.43.x.x` would be
  shadowed by the `10.0.0.0/8` exclusion and fail closed, which is at least the
  safe direction. Verify with an actual `POST /embed` from a tenant pod, not by
  reading the object.
- **`SQUELCH_EMBED_URL` on every tenant**, in `daemon_env`, present only when the
  operator configured one. Same shape as the LLM gateway block: absent rather than
  empty, so the env a warden without the feature builds stays byte-identical and
  the env-contract tests keep their claim. Plus `SQUELCH_EMBED_TOKEN` from the
  fleet Secret, optional, so a tenant provisioned before the Secret existed still
  boots.
- **`SQUELCH_WARDEN_EMBED_URL`** in `config.rs`, `Option<String>`, unset by
  default. **Unset means local embedding**, which is both today's behaviour and
  the self-host answer, and it is why the knob is an Option rather than a boolean
  with a default URL.

**Rollout**, in order: apply the embed objects and wait for the pod; set
`SQUELCH_WARDEN_EMBED_URL` in `15-warden-config.yaml` and restart the warden; let
the roller walk tenants onto the new render one per tick. Memory drops per tenant
as each restarts. **Rollback is unsetting the variable** and rolling again;
tenants rebuild local sessions and the fleet is where it was. Vectors from either
path are in the same space, so nothing needs re-embedding in either direction.

The one ordering trap for the runbook: set the URL **after** the service answers
`/healthz`. A tenant that rolls onto a URL nothing serves builds no local session
(by design, §3) and embeds nothing until the service exists. That is soft (search
degrades to keyword-only, ingest is unaffected, backfill recovers everything
afterwards) and still a window nobody needs to open.

## 6. Self-host

**Unchanged, and that is a non-goal rather than an omission.** A self-host daemon
sets no `SQUELCH_EMBED_URL`, builds its own `FastEmbedder` exactly as today, and
never talks to anything of ours. One user on one machine already has one session,
which is the shape this design is trying to reach; there is nothing to share.
`docs/GETTING-STARTED.md` does not change and self-host gains no new knob.

## 7. Capacity model

**Approximate. These are estimates; measure before believing them.**

`bge-small-en-v1.5` is 12 layers at hidden size 384, so a forward pass costs
roughly `24 * L * d^2` per layer plus attention: about **12 GFLOP at L=256**, and
about **4 GFLOP at L=96**, which is nearer typical mail after the 1000-character
truncation. A shared vCPU here realistically delivers 30-45 GFLOP/s of fp32 GEMM
through ORT, putting one core at roughly **3 sequences/second at full length**
and **8-10/second at typical length**. In practice:

- **Steady state is free.** A tenant receiving 200 messages a day costs about 25
  core-seconds a day. Fifty tenants is roughly 20 minutes of one core per day,
  spread over 24 hours. Queries are one short text each and vanish into the noise.
- **The load is entirely backfills.** A new tenant's first sync is 5,000 to
  20,000 messages, 20 to 60 minutes of one core. That burst is the whole capacity
  question, and it is **signup-shaped**, not steady-state.
- **So the thing to bound is concurrent backfills.** The queue plus
  `INFERENCE_BATCH` is that bound: two simultaneous signups share the service and
  each takes about twice as long; a third gets 429s its backfill retries next
  tick. Nobody is blocked, ingest never stalls, nothing takes the box.

**With cross-tenant batching, the per-node embedding cost is a constant.** One
session, one arena, one weights copy, bounded CPU. Tenant 51 adds Gmail polling
and a SQLite file; it does not add an embedder. That is the sentence this design
is for.

**Run a second replica when** backfills routinely overlap and signups are visibly
slow, or when a second node appears. Two replicas is two sessions and 600-800 MB,
so take it on evidence: watch queue-wait p99 and the 429 rate, not CPU. On more
than one node replicas are also the correctness answer, and the baked-weights
image (§2) is what makes the service node-independent already, where `SETUP.md`
§10's `ReadWriteOnce` model PVC is not.

## 8. Alternative considered: share the weights through the page cache

Convert the model to ONNX **external-data** format, so the 126 MB of weights live
in one file beside a small graph, mount that file read-only into every tenant pod
from the shared model PVC, and set `session.disable_prepacking`. ORT then mmaps
the initializers instead of copying them into the arena, every tenant process
maps the **same** file, and the kernel holds one copy in the page cache.

It is attractive because **per-tenant isolation stays completely intact**: no new
process sees anyone's mail, no NetworkPolicy change, no new image, no new trust
paragraph in `SECURITY.md`, self-host untouched. If it worked it would beat this
design on every axis except CPU. It is not the plan for four reasons:

- **Unverified.** ORT's CPU provider may still copy or pre-pack MatMul
  initializers into arena memory even with prepacking disabled, in which case
  nothing is shared. Nobody has run it.
- **It does not address the scratch.** The +13 MB and +123 MB per run in §1 are
  **activations**, per-process by definition.
- **It does not address the active floor.** The 250 MB is weights plus arena
  high-water mark. Best case this removes 126 MB per tenant, which is real
  (roughly doubling the Phase 1 count) and is not a constant per-node cost.
- **The init container copies the weights today.** `SEED_SCRIPT` does `cp -r
  /models /data/.local/share/squelch/models`, because the root filesystem is
  read-only and fastembed expects to own its cache directory, so every tenant has
  its **own file** and the page cache holds N copies. Sharing needs the daemon to
  read weights from the shared read-only mount: a change to fastembed's cache
  handling, not a flag.

**A half-day spike, not a plan**, and worth running first precisely because a
positive result pushes the trigger out. Convert to external data, mount
read-only, start two daemons, read `Pss` from `/proc/<pid>/smaps_rollup` for
both. Shared pages show up there and nowhere else;
`container_memory_working_set_bytes` cannot answer this question.

## 9. Build trigger and PR plan

**Build when either is true:**

1. The fleet passes **about 15 tenants**. Not 20, because provisioning starts
   failing at 20 and the build plus rollout below is a couple of days.
2. A week of `container_memory_working_set_bytes` per tenant pod, after Phase 1
   and Phase 2 are deployed, disagrees with §1. If idle tenants are not near 40
   MB, or actives are above 250 MB, the 20-tenant number is wrong and the trigger
   moves whichever way the data says.

Before either, run the §8 spike: cheap, and it can move the trigger.

**The PRs, in order.** Each is separately mergeable and none changes fleet
behaviour until the last:

1. **`squelch-embed`, the crate.** Binary, coalescer, bounds, both listeners,
   `Dockerfile.embed`, CI build, weights baked. Tests: bounds rejection, 429 at
   queue-full, positional ordering under coalescing, and a metrics test asserting
   the label set is closed. Merging this ships an image nothing runs.
2. **Warden objects.** `embed_deployment` / `embed_service` /
   `embed_network_policy`, the tenant egress delta, `daemon_env` gaining
   `SQUELCH_EMBED_URL` and `SQUELCH_EMBED_TOKEN`, `SQUELCH_WARDEN_EMBED_URL` in
   config with its http-is-allowed validator. Tests in the env-contract style:
   knob unset, every rendered object byte-identical to today's.
3. **Daemon `RemoteEmbedder`.** The trait impl, the `build_embedder` branch, the
   readiness wording, and the `hybrid_search` degrade-on-error fix from §3, which
   belongs here because it is only reachable once an embedder can fail
   transiently.
4. **`docs/SECURITY.md`.** The subsection described in §4, plus a line in
   `docs/HOSTED.md` naming the second component that sees tenant bodies.
5. **Rollout.** A `deploy/hosted/SETUP.md` section shaped like §10 (apply, verify
   from inside a tenant pod, set the knob, watch the roller), and the
   commented-out entry in `15-warden-config.yaml`.

## Non-goals

Written down so nobody builds them by accident:

- **Not a multi-tenant embedding platform.** One model, one dimension, one
  sequence length, no model selection on the wire: daemon and service agree on
  `bge-small-en-v1.5` at 384 dims and 256 tokens by reading the same constants,
  and a request cannot ask for anything else.
- **No caching, no dedup, no persistence, no per-tenant identity** (§4). The
  service cannot name a tenant, and no metric, log line or credential may give it
  the ability.
- **No GPU**, and **no self-host change** (§6).
- **Not a step toward fleet mode.** `docs/HOSTED.md` step 3 (one process hosting
  N tenants) is a different change with a different trade. This one deliberately
  keeps process-per-tenant, which is the isolation story hosted sells, and moves
  out the one component that has no tenant-specific state in it at all.
