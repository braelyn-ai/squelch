# One embedding session per node

Status: designed 2026-08-26, **not built**; the build trigger is §9. The design
record for the change that makes hosted tenant count independent of the ONNX
Runtime session: one embedder per node, shared by every tenant, instead of one
per tenant pod. Written now and built later because the Phase 1 daemon work
(idle unload, heap trim) buys headroom, and these decisions are cheaper to argue
about before there is code.

## 1. Why

Every hosted tenant runs its own `squelchd` pod on `carrier` (Hetzner CPX21, 3
shared vCPU, 3.8 GB, 80 GB, no swap). Each embeds mail for semantic recall with
fastembed over ONNX Runtime, `bge-small-en-v1.5` at 384 dimensions, weights
pinned to `Xenova/bge-small-en-v1.5` fp32.

**The fleet number is a range, not a number.** On 2026-08-26 the four tenant
pods read **123, 293, 349 and 545 MB** RSS (`deploy/hosted/PRODUCTION.md:30` on
PR #146's branch); a later read of the same four gave 91, 317, 376 and 509. PR
#152 walked one process from boot: ~25 MB before any embed, +195 MB when the
session loads, **~520 MB after one backfill batch pass**, back to ~126 MB when
the session is dropped and ~40 MB after a `malloc_trim`. The Rust side is 25-40
MB of any of those, so the session is 85-90% of a pod that has embedded
anything, and the spread from 91 to 545 MB is what the arena does over a pod's
life rather than measurement noise. Roughly 126 MB is the fp32 weights; the rest
is ORT's arena and the activation buffers it allocates on the first run and
retains. PR #144 measured that per-run half, which is where §2's numbers come
from:

| | `max_length` 512 | `max_length` 256 |
|---|---|---|
| batch-8 pass | +324 MB | +123 MB |
| batch-1 pass | +44 MB | +13 MB |

**Two gates decide the tenant count, and conflating them is how the box got
OOM-killed.** The ResourceQuota in `deploy/hosted/70-tenant-limits.yaml` is
admission control at the API server: `requests.memory`, and separately `pods:
"25"` and `persistentvolumeclaims: "25"` (lines 79 and 82). The **node** is the
scheduler, admitting a pod only if its request fits allocatable, which today is
the whole 3.72 GiB because k3s reserves nothing and which PR #151 takes to **~2.4
GiB** (`PRODUCTION.md:22` on that branch). `requests.memory: 5Gi` on a box with
under 4 GB refuses nobody, so the kernel refused instead: 2026-08-19, four
tenants, two dead `squelchd` processes. Note which way an honest request moves
the count: **down**.

- **After #146 and #151, five or six.** The per-tenant request goes to 384Mi and
  the quota to `2560Mi`: six by the quota's own arithmetic (`6 x 384Mi` fits, `7
  x 384Mi` does not), five or six by the scheduler once the monitoring agent,
  Traefik, CoreDNS and the warden come out of ~2.4 GiB. The quota is the gate
  that should bind, because its refusal is a provision that fails visibly rather
  than a mailbox the kernel picks. Idle unload (#152) is what makes a request
  that honest defensible, taking an idle tenant to ~40 MB; it does not make the
  active case cheaper.
- **With one shared session**, a tenant pod is 40-50 MB including the HTTP
  client, against one 300-400 MB service on the node: `(2400 - 400) / 50` is
  about forty. Memory stops being the gate and `pods: "25"` and
  `persistentvolumeclaims: "25"` become it, which is a one-line edit §9 carries.

The win survives every one of those numbers moving: today the ONNX cost is `N x
300-500 MB`, after this it is one service plus a few MB per tenant, so **the ONNX
line leaves the capacity arithmetic** and what is left is storage, the PVC cap
and 3 vCPU of Gmail poll loops.

## 2. Shape

### The unit: a new crate and its own image

`squelch-embed`, a new workspace member with one small binary, built by a new
`Dockerfile.embed`. The alternative was a `squelchd embed-serve` subcommand
reusing the daemon image, which keeps the image count at one. Two reasons it
loses:

1. **Release cadence.** The daemon image is what the fleet roller pins and walks
   tenants onto one per tick. Making the shared embedder a mode of that image
   ties every tenant's embedding availability to a rollout mechanism designed to
   move slowly and refuse to proceed past a casualty.
2. **What is in the process.** The binary every tenant's mail text flows through
   should have a one-sentence description. `squelch-embed` opens no store,
   mounts no credential, and has no Gmail or LLM client on any route. It
   therefore does **not** link `squelch-core`, which pulls `rusqlite`,
   `sqlite-vec`, `keyring`, `age`, `oauth2`, `mail-parser`, `ammonia` and
   `reqwest` (`squelch-core/Cargo.toml:13-49`) and makes that sentence
   unprovable to whoever audits it. Depend on `fastembed` directly, or put the
   `Embedder` trait in a leaf crate both sides use. Linking is not reachability,
   but the audit is cheaper when the two agree.

A third reason, **weights**, was in the first draft and was wrong twice. It said
`deploy/hosted/SETUP.md` §10 "deliberately keeps the daemon image weight-free";
SETUP.md:728-733 says the opposite, that the answer for a second node is to bake
weights into a squelchd image and point `SQUELCH_WARDEN_IMAGE` at it. It also
called embedding "a feature self-host does not use", and self-host builds a
`FastEmbedder` locally (§6). What survives decides nothing: self-host resolves
weights at runtime, so weight bytes in a daemon image are pull size there with no
benefit. The service bakes its own regardless, which is what lets it need no
model PVC and no egress (§4).

The cost is one more image in CI and one more thing to roll.

### The objects

- **One Deployment**, `replicas: 1`, in the **`tenants`** namespace, because the
  warden's RBAC is namespace-scoped and a namespace of its own means a wider
  warden token (§5). Not `warden` either, which is deliberately unlabelled for
  Pod Security Admission (its pod carries an API token); the embedder should run
  under `restricted` like a tenant does, and `tenants` already is.
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
every response, and §3 says what the daemon must do with it, which is not what
the first draft assumed.

| Status | When | What the daemon does |
|---|---|---|
| 400 | shape violated | logs, gives up on that text (a bug, not a transient) |
| 429 | lane full, with `Retry-After` | drops the batch; backfill recovers it, with backoff (§3) |
| 503 | session not built yet | same as 429 |
| 5xx | anything else | same as 429 |

**Timeouts, both ends.** `RemoteEmbedder` sets a 2 s connect timeout and a
request timeout (5 s for one text, 30 s for a batch), and it is not optional: the
embed call is local CPU work inside `spawn_blocking` today
(`squelch-core/src/sync/mod.rs:1304`) and becomes a network call at that exact
site, where unbounded means a blocking-pool thread parked forever on a half-open
connection, one per poll tick, until the pool is gone and the daemon has stopped
syncing. The server sets read, write and header timeouts of the same order so a
slow caller cannot hold queue slots. **On a timeout, an abandoned request or a
dropped connection the texts were in memory only**: they are dropped, never
written anywhere, and the coalescer discards a queued entry whose caller has
gone. Nothing about a failed request is retained, which is the claim §4 makes
about a successful one.

### Bounds, and which one is the memory bound

- **`MAX_TEXTS_PER_CALL = 8`.** The daemon's largest batch after PR #148 is
  `embed.backfill_batch`, whose default drops from 64 to 8
  (`squelch-core/src/config.rs:782` on that branch); ingest and query send one
  text each. A wire bound that forces the daemon to re-chunk drifts from the
  daemon's config, so the wire takes the daemon's number.
- **`INFERENCE_BATCH = 8`. This is the memory bound.** From §1, batch-8 at 256
  tokens costs +123 MB of scratch. The batch-64 figure everyone quotes, +1.7 to
  +2.7 GB, is at 512 tokens (PR #148's table) and this service pins 256, so the
  number that applies is batch-64 at 256: scaled by the same 123/324 ratio,
  roughly **0.6 to 1.0 GB**, still several times the pod limit below. The cap
  survives with a number that is about this service, and it **is** the memory
  ceiling, so it is a constant with that sentence next to it.
- **The two are equal, and that is a consequence, not a coincidence.** A request
  therefore never splits; the coalescer only merges. Say that plainly rather than
  promising batching as a general speedup: a full 8-text backfill request fills a
  batch by itself, so cross-tenant batching buys a forward pass only on **real
  overlap inside the window**, queries meeting queries or a short backfill tail
  meeting a query. If `backfill_batch` rises again, either this rises with it or
  the coalescer grows a splitter; today it must not have one.
- **`MAX_CHARS_PER_TEXT = 8192`**, and a 1 MB body limit: a sanity bound, not a
  product decision. Ingest and backfill truncate to `embed.max_chars` (1000 after
  #144) through `message_embed_text` (`squelch-core/src/sync/mod.rs:1199` and
  `:1350`). **Queries do not.** `squelch-api/src/handlers.rs:804` hands the raw
  search term to `hybrid_search`, which calls `embedder.embed(query_text)`
  untruncated (`squelch-core/src/store/sqlite/search.rs:201`); #144 says so out
  loud, because today `max_tokens` alone bounds a long query. So `RemoteEmbedder`
  truncates to `max_chars` client-side, **and** the service truncates rather than
  rejecting on length: a search box is where a user can paste anything, and a 400
  there is a 500 in the client.
- **`MAX_QUEUED_TEXTS = 256`**, split across the two lanes below. Past a lane's
  share, 429.

Sequence length is fixed at the daemon's `max_tokens` (256 after PR #144),
because the two must produce vectors in the same space and `max_length` changes
what the model reads. If that number moves it moves in both places in one commit,
or existing vectors stop being comparable.

### Two lanes, because a search must not queue behind a signup

Requests wait up to **10 ms** in a coalescing queue, then whatever has arrived
(capped at `INFERENCE_BATCH`) goes through as one ORT run; a batch fires when it
is full or the window closes. 10 ms is invisible at query time, where a search
already costs tens of ms of SQLite work.

The queue is **two queues, and the interactive one drains first.** §7's own
throughput is why: at 3 to 8 sequences per second on a shared core, 256 queued
texts is **30 to 85 seconds** of work, so without a lane one tenant's signup
backfill parks every other tenant's search behind a minute of somebody else's
mail. The lane is chosen by **request shape**, single-text interactive and
multi-text backfill, because the service cannot name a tenant (§4) and must not
need to; the one misclassification, a backfill whose last batch has one row left,
costs a single forward pass of queue-jumping. Each lane has its own share of
`MAX_QUEUED_TEXTS` (192 backfill, 64 interactive), so a backfill flood cannot 429
a search and a search flood cannot stall a backfill indefinitely.

Two tenants' texts sit in the same ORT batch. That is the design, and §4 is what
makes it acceptable: the batch is a transient positional array, results return by
index to the connection that supplied them, and nothing survives the run.

Pod resources: `requests: 320Mi / 200m`, `limits: 768Mi / 2000m`, both out of the
`tenants` quota (§5). The request covers weights plus one batch of scratch; the
limit leaves room for the arena's high-water mark without letting a bug take the
node.

## 3. The daemon side

A `RemoteEmbedder` in `squelch-core/src/embed/`, implementing the existing
`Embedder` trait. `embed` posts one text, `embed_batch` posts up to
`MAX_TEXTS_PER_CALL` per call and chunks past that, both truncating to
`max_chars` first, and `dims` returns the configured 384 without a network call.

**Because `dims` is a constant, the echoed `dims` is what must be checked.**
`SqliteStore::attach_embedder` compares `embedder.dims()` to `VEC_DIMS`
(`squelch-core/src/store/sqlite/mod.rs:192`), which against a constant is 384 ==
384 and proves nothing about what the service returns. So `RemoteEmbedder`
**rejects any response whose `dims`, or whose vector length, disagrees with its
configured dims**, before the vector reaches the store. The vec0 width checks in
`knn_by_vector` and `upsert_message_vector`
(`squelch-core/src/store/sqlite/search.rs:143` and `:504`) are the last line and
do catch it, but as a per-row error mid-backfill, which is a bad place to learn
the service was rebuilt on another model.

`SQUELCH_EMBED_URL` selects it. **When it is set, `FastEmbedder::new` is never
called** and no ONNX session is built in the daemon process. That single fact is
the entire saving, so it lives in one place: `build_embedder`
(`squelchd.rs:439`) branches on the URL and the two arms are mutually exclusive.
No fall-back-to-local, because a daemon that quietly builds a local session when
the service is unreachable is a daemon that quietly costs 300-500 MB.

**Both call sites take that branch**, and PR #152 changes what the local arm
hands back: a concrete `Arc<LazyEmbedder>`, eagerly loaded, because both callers
then spawn a reaper against the concrete type (`squelchd.rs:1450` in
`run_daemon`'s sync path, `:2203` in `serve`'s background build task, on that
branch). The remote arm builds **no `LazyEmbedder` and spawns no reaper**: there
is no session to unload, and idle unload is the problem this design deletes
rather than tunes. So the reaper spawn belongs inside the local arm rather than
beside the call, and the remote path returns `Arc<dyn Embedder>`.

**Readiness: settled means attempted, not healthy.** `squelchd.rs:1511-1516`
already spells this out, SETTLED and not succeeded, so that an embedder which can
never build leaves a daemon that syncs, serves both doors and searches by
keyword; `:1531-1533` names the second reason, that answering 503 pulls the pod
out of its own Service and stops the fleet roller dead. With a URL configured,
settled means **the remote embedder was attempted once, whichever way it
resolved**. A dead service is soft: search degrades to keyword-only, ingest is
unaffected, backfill recovers afterwards, the roller keeps moving. That bit is
load-bearing twice after PR #148, which makes `embedder_settled` a
`watch::Sender<bool>` (`squelchd.rs:1553` on that branch) the first backfill
waits on through `SyncEngine::with_embedder_gate`. The gate waits for **the
attempt, not for success**, opening on a failed init exactly as on a good one and
bounded by `EMBEDDER_GATE_CEILING`, 15 minutes
(`squelch-core/src/sync/mod.rs:123`); with a remote embedder the attempt is one
`/healthz` request, so it opens in milliseconds rather than after a 126 MB model
download, which is the case it was written for.

Failure semantics are today's local-failure semantics, unchanged:

- **Ingest** (`SyncEngine::embed_and_store`, `sync/mod.rs:1297`) already logs a
  redacted line and returns; the vector is recovered by
  `backfill_missing_vectors` later. Ingest never blocks on the embedder.
- **Backfill** (`backfill_missing_vectors`) already logs and stops the pass on a
  batch failure (`sync/mod.rs:1360-1369`), retrying next tick. A 429 is exactly
  that case, and the recovery is real.
- **Query** needs one real change, the one thing here that is a code fix rather
  than a new path. `SqliteStore::hybrid_search` does `embedder.embed(query_text)?`
  and propagates: with a local embedder that effectively never fails once built,
  with a remote one it fails on any network blip and a user's search 500s instead
  of degrading. `hybrid_search` must treat an embed **error** the way it treats
  an absent embedder: no vector leg, keyword results, one redacted log line.
  `semantic_search` keeps propagating, having nothing to degrade to.

**A 429 needs backoff, because nothing honours `Retry-After` today.** A stopped
pass resumes on the next poll tick and `sync.poll_secs` defaults to **5**
(`squelch-core/src/config.rs:206`), so a 429'd fleet re-posts every five seconds
per tenant: a service that just said "I am full", asked again twelve times a
minute by everyone. The daemon-side PR adds exponential backoff on 429 and 503 in
the sync engine (5 s, doubling to a few minutes, reset on success), with
`Retry-After` as the floor rather than a header nobody reads.

## 4. Security

Be blunt about what this is. **`squelch-embed` is a process we operate that sees
the subject and body text of every hosted tenant's normal-sensitivity mail, in
flight, in one address space.** That widens the hosted trust story and belongs in
`docs/SECURITY.md`, not only here. It is not new **in kind**: Bifrost already
sees every message Stage 2 triages, and `docs/HOSTED.md` says so. This is a
second such component with a narrower job.

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
  that is unlabelled totals plus closed `outcome` (`ok`, `rejected`,
  `queue_full`, `error`) and `lane` labels. A per-tenant counter would put the
  fleet's mail volume, tenant by tenant, on an unauthenticated scrape.
- **Never log, label a metric by, cache by, or rate-limit by peer address.** The
  one the first draft missed, and the control the token paragraph below depends
  on: a per-peer series is a per-tenant series with one extra lookup. If per-peer
  fairness is ever wanted, key it on an opaque value derived at process start,
  held in memory, never persisted and never exported.

**Authentication: NetworkPolicy is the boundary, and add a shared token anyway.**

Reachability is the strong control. A tenant's NetworkPolicy is default-deny with
two egress rules: CoreDNS, and `0.0.0.0/0:443` minus `BLOCKED_EGRESS_CIDRS`,
which excludes `10.0.0.0/8` and so every in-cluster pod and Service. Today a
tenant pod **cannot reach the embed service at all**; after §5 it reaches exactly
that pod selector on exactly that port. The service's own NetworkPolicy admits
ingress only from pods carrying `app.kubernetes.io/managed-by=squelch-warden`,
plus the monitoring agent on the metrics port. Egress is denied outright: with
the weights baked in the service has nothing to dial, so a compromised embedder
cannot ship text anywhere.

**Be exact about what a shared token is worth.** Against a compromised tenant,
nothing: a compromised tenant holds the token. It buys one thing, a non-tenant
workload landing on the pod network when the CNI is not enforcing policy, and
that failure is real enough to already have a test.
`deploy/hosted/SETUP.md:656-665` runs a pod that tries to open a tenant's port
and asserts `blocked`, where `REACHABLE` means the CNI is ignoring NetworkPolicy
and nothing else is worth doing until it is fixed. The token is the belt under
that, and a 401 is a better answer, and a better metric, than serving something
that arrived by accident. `SQUELCH_EMBED_TOKEN`, one fleet-wide value in a
Secret, constant-time compare.

One shared value, not per-tenant, and the honest reason is narrower than the
first draft's. It is **not** that the service cannot distinguish callers: every
request arrives from a distinct, stable pod IP and no design choice changes that.
It is that **no durable tenant name or account identifier ever reaches the
service**, in the path, a header or the body, so nothing it holds can be joined
to a mailbox later. A per-tenant credential would hand it the durable name it is
otherwise denied, and the peer-address rule above is what stops the accidental
one from becoming durable.

**TLS: no. Plaintext HTTP on the pod network**, because the traffic never leaves
the node and TLS means a certificate to mint, rotate and expire on a path whose
only observer would already hold the node's network namespace and could read the
SQLite files directly. Compare `SQUELCH_WARDEN_LLM_BASE_URL`, which the warden's
validator **requires** to be https (`squelch-warden/src/config.rs:880`) and
correctly: Bifrost is remote and a virtual key crosses the public internet. This
service is neither, so `SQUELCH_WARDEN_EMBED_URL`'s validator is the mirror image
and should say so at the check: `http://` is allowed **because** the destination
is in-cluster, and a URL with a public host is refused outright.

**What `docs/SECURITY.md` gains.** A subsection under §4: hosted mail text
reaches two processes we operate, the LLM gateway and the embedding service;
sealed mail reaches neither, enforced for both at the same store-side
`sensitivity == Normal` gate; the embedding service logs nothing, stores nothing,
caches nothing, is never told a tenant's name, and cannot reach the network;
self-host reaches neither.

## 5. Warden changes

The warden stays the only thing that touches Kubernetes, and every object stays a
typed `k8s-openapi` struct.

- **New objects in `objects.rs`**: `embed_deployment`, `embed_service`,
  `embed_network_policy`, built the way the tenant ones are (same
  `PodSecurityContext` and `SecurityContext`, `automount_service_account_token:
  false`, `enable_service_links: false`). They are **fleet objects, not tenant
  objects**: applied once at warden startup or by an explicit `squelch-warden
  embed apply`, not per tenant, and not touched by the roller's drift pass.
- **In `tenants`, and RBAC is why.** `deploy/hosted/10-warden-rbac.yaml` is a
  Role and a RoleBinding in `tenants` and nothing else (lines 57-119), bound to a
  ServiceAccount in `warden`. A service in an `embed` namespace of its own needs
  a **second Role and RoleBinding there**, widening the warden's token, which
  includes `pods/exec`, to a second namespace. All that buys is tidier
  bookkeeping for `pods: "25"`, whose comment calls itself the tenant count.
  Trade a comment for a wider token and the comment wins: the service lives in
  `tenants`, that comment says one of the 25 pods and 320Mi of `requests.memory`
  are the embedder, and the RBAC file is untouched. If anyone later moves it to
  its own namespace, the same PR adds the Role and RoleBinding.
- **NetworkPolicy delta on every tenant**: one more egress rule, to the embed
  service's pod selector, TCP 8850. The rule names the **pod selector, not the
  ClusterIP**, and the reason is kube-proxy rather than rule precedence:
  kube-proxy DNATs a Service address in `nat/PREROUTING` and k3s's kube-router
  (`SETUP.md:607`) evaluates NetworkPolicy in `filter/FORWARD` afterwards, so by
  the time a rule is consulted the destination is already the backend pod IP and
  a rule against `10.43.x.x` matches nothing. Not, as the first draft had it,
  because the `except: 10.0.0.0/8` inside the `0.0.0.0/0:443` rule would shadow
  it: NetworkPolicy rules are additive and an `except` subtracts only from its
  own `ipBlock`. Verify with an actual `POST /embed` from a tenant pod, not by
  reading the object.
- **`SQUELCH_EMBED_URL` on every tenant**, in `daemon_env`, present only when the
  operator configured one. Same shape as the LLM gateway block: absent rather
  than empty, so the env a warden without the feature builds stays byte-identical
  and the env-contract tests keep their claim. Plus `SQUELCH_EMBED_TOKEN` from
  the fleet Secret, optional, so a tenant provisioned before the Secret existed
  still boots.
- **`SQUELCH_WARDEN_EMBED_URL`** in `config.rs`, `Option<String>`, unset by
  default. **Unset means local embedding**, both today's behaviour and the
  self-host answer, which is why the knob is an Option and not a boolean with a
  default URL.

**Rollout**, in order: apply the embed objects and wait for the pod; set
`SQUELCH_WARDEN_EMBED_URL` in `15-warden-config.yaml` and restart the warden; let
the roller walk tenants onto the new render one per tick. Memory drops per tenant
as each restarts. **Rollback is unsetting the variable** and rolling again;
tenants rebuild local sessions and the fleet is where it was. Vectors from either
path are in the same space, so nothing needs re-embedding either way.

The ordering trap for the runbook: set the URL **after** the service answers
`/healthz`. A tenant that rolls onto a URL nothing serves builds no local session
(by design, §3) and embeds nothing until the service exists. That is soft in
exactly the sense §3 gives the word, the attempt resolving either way so
readiness settles and the roller keeps moving, and still a window nobody needs to
open.

## 6. Self-host

**Unchanged, and that is a non-goal rather than an omission.** A self-host daemon
sets no `SQUELCH_EMBED_URL`, builds its own `FastEmbedder` exactly as today, and
never talks to anything of ours. One user on one machine already has one session,
which is the shape this design is trying to reach; there is nothing to share.
`docs/GETTING-STARTED.md` does not change and self-host gains no new knob.

## 7. Capacity model

**Approximate. These are estimates; measure before believing them.**

A forward pass of a 12-layer, hidden-384 model is about **12 GFLOP at L=256** and
**4 GFLOP at L=96** (nearer typical mail after the 1000-character truncation),
against the 30-45 GFLOP/s of fp32 GEMM a shared vCPU delivers through ORT: **3
sequences/second at full length**, **8-10/second at typical**. In practice:

- **Steady state is free.** A tenant receiving 200 messages a day costs about 25
  core-seconds a day; even fifty tenants is 20 minutes of one core per day spread
  over 24 hours. Queries are one short text each and vanish into the noise, which
  is also why giving them their own lane costs nothing.
- **The load is entirely backfills.** A new tenant's first sync is 5,000 to
  20,000 messages, 20 to 60 minutes of one core. That burst is the whole capacity
  question, and it is **signup-shaped**, not steady-state.
- **So the thing to bound is concurrent backfills.** The backfill lane plus
  `INFERENCE_BATCH` is that bound: two simultaneous signups share the service and
  each takes about twice as long; a third gets 429s its backfill retries with
  backoff (§3). Nobody is blocked, no search waits behind it, ingest never
  stalls, nothing takes the box.

**With cross-tenant batching, the per-node embedding cost is a constant.** One
session, one arena, one weights copy, bounded CPU. Tenant 51 adds Gmail polling
and a SQLite file; it does not add an embedder. That is the sentence this design
is for.

**Run a second replica when** backfills routinely overlap and signups are visibly
slow, or when a second node appears. Two replicas is two sessions and 600-800 MB,
so take it on evidence: watch queue-wait p99 per lane and the 429 rate, not CPU.
The service's baked weights already make it node-independent, the same move
`SETUP.md`:728-733 prescribes for a daemon image on a second node, where the
`ReadWriteOnce` model PVC stops working.

## 8. Alternative considered: share the weights through the page cache

Convert the model to ONNX **external-data** format so the 126 MB of weights live
in one file beside a small graph, mount that file read-only into every tenant pod
from the shared model PVC, and set `session.disable_prepacking`. ORT then mmaps
the initializers instead of copying them into the arena, every tenant process
maps the **same** file, and the kernel holds one copy.

It is attractive because **per-tenant isolation stays completely intact**: no new
process sees anyone's mail, no NetworkPolicy change, no new image, no new trust
paragraph in `SECURITY.md`, self-host untouched. If it worked it would beat this
design on every axis except CPU. Four reasons it is not the plan. **It is
unverified**: ORT's CPU provider may pre-pack MatMul initializers into arena
memory even with prepacking disabled, and nobody has run it. **It does not
address the scratch**, because §1's +13 and +123 MB per run are activations, and
those are per-process by definition. **It does not address the active floor**,
because the 300-500 MB is weights plus arena high-water mark and this removes at
best the 126 MB of weights. And **the init container copies the weights today**:
`SEED_SCRIPT` does `cp -r /models /data/.local/share/squelch/models`
(`squelch-warden/src/objects.rs:317`) because the root filesystem is read-only
and fastembed expects to own its cache directory, so every tenant has its own
file and the page cache holds N copies. Sharing needs fastembed to read from the
shared mount, a change to its cache handling and not a flag.

Note what costing this runs into first: **production has never had the model PVC
at all.** `SQUELCH_WARDEN_MODEL_PVC` is commented out
(`15-warden-config.yaml:209`) and PR #149 confirms no `squelch-models` claim
exists, so today every tenant downloads the weights from Hugging Face on first
boot and there is no shared file to start from.

**A half-day spike, not a plan**, and worth running first precisely because a
positive result pushes the trigger out. Convert to external data, mount
read-only, start two daemons, read `Pss` from `/proc/<pid>/smaps_rollup` for
both. Shared pages show up there and nowhere else;
`container_memory_working_set_bytes` cannot answer this question.

## 9. Build trigger and PR plan

**Build when either is true:**

1. **The fleet reaches four or five tenants against #146's six.** Not the "about
   15" this document first said, which came from the 20-tenant arithmetic §1 now
   retracts. Past the quota the choices are a bigger box, a second node, or this,
   and the build plus rollout below is a couple of days.
2. **A week of `container_memory_working_set_bytes` per tenant pod disagrees with
   §1**, evaluated **after #152 and #146 are deployed** and not before, because
   until then it measures a fleet nobody has fixed. The week is the point: §1's
   spread from 91 to 545 MB is point reads, which cannot tell an idle tenant from
   an active one, and the whole Phase 1 case is that those differ by 10x. If idle
   tenants are not near 40 MB, or actives above 300 MB, the trigger moves
   whichever way the data says.

Before either, run the §8 spike: cheap, and it can move the trigger.

**The PRs, in order.** Each is separately mergeable and none changes fleet
behaviour until the last:

1. **`squelch-embed`, the crate.** Binary, two-lane coalescer, bounds, timeouts,
   both listeners, `Dockerfile.embed`, CI build, weights baked, no `squelch-core`
   dependency. Tests: over-length truncation rather than 400, 429 at lane-full,
   positional ordering under coalescing, an interactive request overtaking a
   queued backfill, and a metrics test asserting the label set is closed and
   carries no peer address. Merging this ships an image nothing runs.
2. **Warden objects.** `embed_deployment` / `embed_service` /
   `embed_network_policy` in `tenants`, the tenant egress delta, `daemon_env`
   gaining `SQUELCH_EMBED_URL` and `SQUELCH_EMBED_TOKEN`,
   `SQUELCH_WARDEN_EMBED_URL` in config with its http-is-allowed validator. Tests
   in the env-contract style: knob unset, every rendered object byte-identical to
   today's. No RBAC change, deliberately (§5).
3. **Daemon `RemoteEmbedder`.** The trait impl with client-side truncation, the
   response-`dims` rejection, the client timeout, backoff on 429 and 503, the
   `build_embedder` branch that builds no `LazyEmbedder` and spawns no reaper,
   the readiness wording, and the `hybrid_search` degrade-on-error fix from §3,
   which belongs here because it is only reachable once an embedder can fail
   transiently.
4. **Quota and monitoring.** In `70-tenant-limits.yaml`, raise `pods` and
   `persistentvolumeclaims` past 25, which become the binding gate once memory
   stops being it (§1), and account for the embed pod in the comments. In
   `80-monitoring.yaml`, a scrape job for the service: the `squelchd` job cannot
   find it, being scoped to `namespaces: names: [tenants]` (lines 273-274) and
   then keeping only pods labelled `app.kubernetes.io/name: squelchd`. Applying
   the ConfigMap is not enough, because the agent runs without
   `--web.enable-lifecycle` (lines 329-332): rollout-restart it and check the
   target appears, or the panels stay blank and blank reads as no traffic.
5. **`docs/SECURITY.md`.** The subsection described in §4, plus a line in
   `docs/HOSTED.md` naming the second component that sees tenant bodies.
6. **Rollout.** A `deploy/hosted/SETUP.md` section shaped like §10 (apply, verify
   with a real `POST` from inside a tenant pod, set the knob, watch the roller),
   and the commented-out entry in `15-warden-config.yaml`.

## Non-goals

Written down so nobody builds them by accident:

- **Not a multi-tenant embedding platform.** One model, one dimension, one
  sequence length, no model selection on the wire: daemon and service agree on
  `bge-small-en-v1.5` at 384 dims and 256 tokens by reading the same constants,
  and a request cannot ask for anything else.
- **No caching, no dedup, no persistence, no per-tenant identity** (§4). The
  service is never told a tenant's name, and no metric, log line, credential or
  peer-keyed counter may give it one.
- **No GPU**, and **no self-host change** (§6).
- **Not a step toward fleet mode.** `docs/HOSTED.md` step 3 (one process hosting
  N tenants) is a different change with a different trade. This one deliberately
  keeps process-per-tenant, which is the isolation story hosted sells, and moves
  out the one component that has no tenant-specific state in it at all.
