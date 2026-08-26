# Hosted Passband: a fresh Debian box to the first tenant

This is the runbook for the cluster half of hosted Passband. The other half,
`squelch-control`, runs on Railway and is not installed here.

## What you are building

```
                    signup.passband.app                warden.passband.app
                          |                            |
                    (CNAME to Railway)            (A to this box)
                          |                            |
                   squelch-control  ---- HTTPS ---> Traefik --> squelch-warden
                   (Railway)          bearer token             (pod, ns warden)
                          |                                          |
                          |                                          | kube API
                          |                                          v
                          |                        ns tenants: Secret, Secret, PVC,
                          |                        NetworkPolicy, Service, Deployment,
                          |                        Ingress  -- one set per tenant
                          |                                          |
                          '--- age ciphertext -----------------------'
                              (sealed to THIS TENANT's recipient;
                               only this tenant's pod can open it)

  <label>.<base> --(A to this box)--> Traefik --> Service <label> --> squelchd pod
```

Three things hold, and they are the whole pitch:

- The **control plane** runs the Google web client. For each tenant it learns a
  **recipient** (a public age key) from the warden and seals that tenant's
  credentials to it the moment the OAuth exchange returns. It holds no identity,
  for any tenant, ever.
- The **warden** mints each tenant's identity in memory and writes it straight
  into that tenant's Secret. It never logs it, never stores it anywhere else,
  and forgets it. Its RBAC is one Role in one namespace, it cannot delete a
  volume or a credential, and the one Secret it may delete is the identity of a
  tenant that never finished signing up.
- **Each tenant's pod** mounts its own identity and is the only thing in the
  world that can decrypt that tenant's credentials.

There is no box-wide key. Losing one tenant's Secret costs that tenant a
re-consent with Google and costs nobody else anything.

## 0. What you need

- A Linux box that runs k3s: Debian 13 or Ubuntu 24.04 both work — pick the one
  you can operate half-asleep. The daemon's glibc floor (`ort` needs 2.38+)
  lives INSIDE the squelchd container image, not on the host; the old "trixie
  is not optional" rule died with the systemd design. What the host does need
  is a kernel with idmapped mounts (6.3+) for the user-namespace pods — trixie
  ships 6.12, Ubuntu 24.04 ships 6.8, both fine. 2 vCPU and 4 GB is a sensible
  floor for a handful of tenants; each tenant pod is a sync loop plus an ONNX
  embedder.
- Two domains, on purpose. The tenant base domain (`passband.email` here) means
  exactly one thing: a wildcard subdomain is a tenant, full stop. Product and
  internal surfaces (signup, the warden) live on the product domain
  (`passband.app` here) so nothing operator-owned ever squats in the tenant
  namespace. Substitute yours everywhere.
- For the TENANT zone, a DNS provider **cert-manager has a DNS-01 solver for**.
  Not "a provider with an API" — a wildcard can only be issued over DNS-01, and
  cert-manager can only drive the providers it (or a webhook) implements. See
  "Your registrar is probably not your DNS provider" under §1.
- The Railway service for `squelch-control` already created.

## 1. DNS

| Name | Type | Value | Why |
|---|---|---|---|
| `passband.email` | A | your box IP (or the marketing site) | optional |
| `*.passband.email` | A | your box IP | every tenant's subdomain — DNS-only, never proxied |
| `warden.passband.app` | A | your box IP | the control plane's way in |
| `signup.passband.app` | CNAME | your Railway app hostname | signup never touches this box |

The wildcard is what makes a new tenant instant: provisioning creates an Ingress
for `alice.passband.email` and DNS already answers for it.

### Your registrar is probably not your DNS provider

The two domains have different certificate needs, and that decides where each
zone's DNS lives:

- **The tenant zone needs DNS-01**, because it needs a wildcard and nothing else
  issues one. cert-manager has to write the `_acme-challenge` TXT record itself,
  every 60 days, unattended — so this zone must be hosted by a provider
  cert-manager can drive (Cloudflare, Route53, DigitalOcean, Google, Azure,
  AcmeDNS, or a provider webhook).
- **The product zone does not.** `warden.passband.app` is a single host, so it
  issues over the HTTP-01 catch-all solver in `40-wildcard-certificate.yaml`,
  which needs port 80 open to Traefik and no DNS credential at all.
  `signup.passband.app` is a CNAME to Railway, which handles its own TLS. So this
  zone can stay wherever it was registered.

This install is the concrete case: both domains are registered at Namecheap,
which has **no cert-manager DNS-01 solver**. The sanctioned fix is to move only
the tenant zone's DNS to a provider that does — Cloudflare's free tier is enough,
and moving a zone is changing its nameservers at the registrar, not transferring
the domain. The product zone stays put. Registration and DNS hosting are separate
things and there is no reason to move both.

> **If you use Cloudflare: the wildcard record must be DNS-only (grey cloud).**
> Never proxied. The orange cloud puts Cloudflare's TLS termination in front of
> every tenant mailbox — it would be decrypting mail traffic that this whole
> design spends its effort keeping inside one pod — and it hides the box's real
> address from ACME's HTTP-01 fallback while fighting the certificate
> cert-manager just issued. Check it after every zone edit; the dashboard
> defaults new A records to proxied.

## 2. k3s, with secrets encryption

```sh
curl -sfL https://get.k3s.io | INSTALL_K3S_EXEC="\
  --secrets-encryption \
  --write-kubeconfig-mode=0600 \
  --kube-apiserver-arg=feature-gates=UserNamespacesSupport=true \
  --kubelet-arg=feature-gates=UserNamespacesSupport=true \
" sh -

export KUBECONFIG=/etc/rancher/k3s/k3s.yaml
kubectl get nodes
k3s secrets-encrypt status     # should say: Encryption Status: Enabled
```

**`--secrets-encryption` is not optional here, and it cannot be added quietly
later.** Every tenant's private age key lives in a Secret. Without this flag,
k3s stores Secrets as plaintext rows in its sqlite datastore, which means a
copied `/var/lib/rancher/k3s/server/db` is every tenant's mailbox. That would
gut the design: encryption-at-rest for credentials is the reason the age
identities exist at all, and storing the identities in the clear would make the
whole chain decorative.

If you are retrofitting an existing cluster, turn it on and then rewrite the
existing Secrets so they are actually re-encrypted:

```sh
k3s secrets-encrypt enable
systemctl restart k3s
k3s secrets-encrypt reencrypt --force
```

**`UserNamespacesSupport`** is the second flag and it is a nice-to-have rather
than a must. It lets each tenant pod get its own user namespace, so root inside
a tenant's container maps to an unprivileged id on the node. It needs a kernel
with idmapped mounts (6.3+ is comfortable; trixie ships 6.12) and a container
runtime that supports it. If tenant pods fail to start with a `hostUsers`
complaint, set `SQUELCH_WARDEN_USER_NAMESPACES` to `"off"` in
`15-warden-config.yaml`, apply it, restart the warden, and read "What this does
and does not isolate" below to understand what you gave up.

## Tenant data on a block volume

Unnumbered because it is optional, but do it here — between k3s and the first
tenant — or not at all. Repointing storage is free with zero PVCs and a migration
with fifty.

Out of the box, k3s's local-path provisioner carves every tenant PVC out of
`/var/lib/rancher/k3s/storage/`, which is the **root disk**. That is the wrong
disk for mail on any cloud that sells disks the way Hetzner does: a root disk
grows with the instance type and never shrinks back, so one busy month of mail
permanently raises the floor on what the box costs. A block volume is the
opposite shape — attach 50 GB, grow it online when it fills, pay for exactly
that, and keep the mail on something you can detach and re-attach to a
replacement box.

Create the volume in the provider's console (delete protection ON — this is every
tenant's mailbox and the console offers exactly one click between it and
nothing), attach it, then on the box:

```sh
mkfs.ext4 /dev/disk/by-id/<your-volume-device>
mkdir -p /mnt/tenant-data
# fstab, by-id and never by /dev/sdX, which renumbers on reboot:
echo '/dev/disk/by-id/<your-volume-device> /mnt/tenant-data ext4 discard,nofail,defaults 0 0' >> /etc/fstab
mount -a && df -h /mnt/tenant-data
```

`nofail` is not optional: without it, a volume that is slow to attach or has been
detached turns a reboot into a box that never finishes booting.

Then point local-path at it. The durable way is the k3s server flag, because it
is what renders the provisioner's config in the first place:

```sh
# /etc/rancher/k3s/config.yaml is how k3s takes server flags durably: it
# survives upgrades AND re-runs of the install script, which rewrite the unit.
echo "default-local-storage-path: /mnt/tenant-data" >> /etc/rancher/k3s/config.yaml
systemctl restart k3s
kubectl -n kube-system get cm local-path-config -o jsonpath='{.data.config\.json}'; echo
kubectl -n kube-system rollout restart deploy/local-path-provisioner
```

Editing the `local-path-config` ConfigMap by hand does the same thing and takes
effect the same way, but it is a bundled k3s manifest: the next k3s upgrade that
touches `local-storage.yaml` puts the old path back, and the symptom is new
tenants quietly landing on the root disk again. The flag survives upgrades.

Either way this only affects **new** claims. A PV already bound keeps the path it
was created with, which is the other reason to do this before there are any.

Last, make the quota agree with the disk. In `70-tenant-limits.yaml`,
`requests.storage` is the aggregate ceiling on PVCs in the namespace and should be
sized to **this volume**, not to the root disk and not to optimism: local-path
does not enforce a claim's size, so the quota is the only thing standing between
fifty tenants and a full filesystem. Divide the volume by the per-tenant claim
size and you have your tenant count.

Growing it later is online and in that order: expand the volume in the console,
then `resize2fs /dev/disk/by-id/<device>` on the box (no unmount, no downtime),
then raise `requests.storage` and re-apply. Shrinking is not a thing, on any of
the three, which is why 50 GB and grow beats 500 GB and hope.

The thing this does NOT do is back anything up, and it moves the mail out from
under the one thing that did: provider server snapshots image the ROOT disk, and
this volume is now deliberately not the root disk. §11 puts a backup back under
it — Litestream, streaming each tenant's database to R2, age-encrypted — and the
path you choose here is the path that section scans, so pick it now and keep it.
Read
"Backups today, stated honestly" under Operating notes for what each of the two
mechanisms does and does not cover.

## 3. cert-manager and the wildcard certificate

```sh
kubectl apply -f https://github.com/cert-manager/cert-manager/releases/latest/download/cert-manager.yaml
kubectl -n cert-manager rollout status deploy/cert-manager-webhook
```

Then edit `40-wildcard-certificate.yaml`: your ACME email, your domain in three
places, and the DNS-01 solver for your provider. The file ships a Cloudflare
example because it is the one most people have; every other provider is the same
file with a different `solvers` block. Your DNS provider only ever sees a TXT
record on `_acme-challenge`, never a tenant's name.

The DNS credential itself is **not** in that file, deliberately: a token with
edit rights on your zone does not belong in a repository. Create it from the
command line (`acme-dns-token.example.yaml` documents the shape):

```sh
kubectl apply -f deploy/hosted/00-namespaces.yaml

kubectl -n cert-manager create secret generic acme-dns-token \
  --from-literal=api-token="<zone-scoped DNS:Edit token>"

kubectl apply -f deploy/hosted/40-wildcard-certificate.yaml
kubectl -n tenants get certificate passband-wildcard -w   # READY=True
```

`00-namespaces.yaml` also puts Pod Security Admission on `tenants` at
`restricted`, the strictest built-in level. Every tenant object the warden
builds already satisfies it, with tests asserting each field, so this changes
nothing about a tenant pod. It bounds what ELSE can run there: a debug pod, a
stuck job, or a workload created with a stolen warden token is refused at
admission unless it is non-root, capability-free, and off the host namespaces.

A wildcard takes a minute or two the first time while ACME waits for DNS.

## 4. Images

Two images: the daemon every tenant runs, and the warden. CI publishes both on
every `daemon-X.Y.Z` tag (`.github/workflows/release-daemon.yml`), tagged with
the git tag verbatim, so the normal path is to pull `daemon-0.0.1` rather than
build anything. By hand, from a checkout:

```sh
git clone https://github.com/braelyn-ai/squelch && cd squelch

docker build -f squelchd/Dockerfile -t ghcr.io/braelyn-ai/squelchd:daemon-0.0.1 .
docker build -f Dockerfile.warden -t ghcr.io/braelyn-ai/squelch-warden:daemon-0.0.1 .
docker push ghcr.io/braelyn-ai/squelchd:daemon-0.0.1
docker push ghcr.io/braelyn-ai/squelch-warden:daemon-0.0.1
```

Tag them. The warden refuses to start with an untagged tenant image, because an
untagged image means every tenant's daemon silently changes version on any pod
restart, which is an upgrade nobody scheduled and nobody can roll back. Never
reuse a tag either: GHCR lets you move one, `imagePullPolicy: IfNotPresent`
means a node that has seen the name will not re-pull it, and the two together
are a rollout that appears to work and changes nothing.

If the packages are private, create the pull secret and uncomment
`SQUELCH_WARDEN_IMAGE_PULL_SECRET` in `15-warden-config.yaml` (that one is for
TENANT pods; the warden's own pod and the roller's take `imagePullSecrets` in
`20-warden.yaml` and `90-warden-roller.yaml`):

```sh
kubectl -n tenants create secret docker-registry ghcr \
  --docker-server=ghcr.io --docker-username=<user> --docker-password=<PAT>
kubectl -n warden create secret docker-registry ghcr \
  --docker-server=ghcr.io --docker-username=<user> --docker-password=<PAT>
```

(The `warden` namespace needs its own copy: an image pull secret does not cross
a namespace. Add `imagePullSecrets` to the warden Deployment if you use it.)

## 5. The warden's token

```sh
kubectl -n warden create secret generic squelch-warden \
  --from-literal=token="$(openssl rand -base64 32)"
```

Read it back once and put it in the control plane's environment as
`SQUELCH_CONTROL_WARDEN_TOKEN`, alongside
`SQUELCH_CONTROL_WARDEN_URL=https://warden.passband.app`:

```sh
kubectl -n warden get secret squelch-warden -o jsonpath='{.data.token}' | base64 -d; echo
```

That token is the tenant namespace. Treat it the way you would treat a database
password. Rotation is in `warden-token.example.yaml`.

## 6. The Google OAuth client

Every tenant daemon needs the OAuth client id and secret that the control plane
consented with, and it will not start without them.

```sh
kubectl -n tenants create secret generic google-oauth-client \
  --from-literal=client_id="<the web client's id>" \
  --from-literal=client_secret="<the web client's secret>"
```

**Why our client secret is inside tenant pods.** A Google refresh token only
works for the OAuth client that minted it. Hosted signups run through
`squelch-control`'s confidential **web** client, so a daemon holding a tenant's
refresh token and not holding that client's credentials cannot refresh an access
token; `squelchd serve` checks for them at boot and exits without them. It is
the SAME client, from the SAME GCP project, that the control plane uses. Not the
desktop client self-hosters get embedded in their image, and not a second client
of our own: a second one would mint tokens the first cannot refresh.

That is custody hosted already admits to. These daemons are our infrastructure,
running our image, on our box, with the tenant's mail on a volume we control; a
shared client secret in that pod adds nothing to what a hosted tenant is already
trusting us with. Be precise about what it is not, though: on its own it opens
nothing. Reading a tenant's mail still needs that tenant's refresh token, which
is sealed to that tenant's age recipient and openable only inside that tenant's
own pod. The self-host tier is untouched and stays untouched.

The Secret's name is `SQUELCH_WARDEN_OAUTH_SECRET_NAME` on the warden (default
`google-oauth-client`); its two keys are fixed at `client_id` and
`client_secret`. Details and rotation: `google-oauth-client.example.yaml`.

## 6b. LLM triage through the gateway

Tenant daemons never hold our Anthropic key. Each pod gets a **Bifrost virtual
key** with a monthly dollar budget, and sends its unchanged Anthropic-wire
traffic to a Bifrost gateway we run on Railway; Bifrost holds the real key,
swaps it in, meters spend per tenant, and refuses a tenant that has blown its
budget. The daemon's own daily call caps are the second, inner layer. Notably
there is **no cluster-side operator secret for any of this**: the per-tenant
`<label>-llm` Secrets are written by the warden when the control plane mints
keys, and the real key lives only in the Railway service's environment.

The gateway, on Railway:

1. New service `bifrost` from this repo, built by `Dockerfile.bifrost`. Set
   `railway.bifrost.toml` as the service's **Config-as-code file path** before
   the first deploy — the root `railway.toml` pins the relay's Dockerfile and
   config-as-code outranks the `RAILWAY_DOCKERFILE_PATH` variable, so skipping
   this ships the relay image (again).
2. Volume mounted at `/app/data`. This is the governance state — every virtual
   key, budget, and recorded cent of spend. Losing it revokes the whole fleet's
   keys at once, so treat it like a database, not a cache.
3. Environment: `APP_HOST=0.0.0.0`, `APP_PORT=8080`, and `ANTHROPIC_API_KEY` —
   the only place the real key lives, anywhere.
4. Generate the service's public domain (port 8080). Tenant egress already
   allows any public 443 host, so the cluster needs nothing.
5. First boot: enable auth with an admin credential (`PUT /api/config` with
   `auth_config` — or the web UI) and turn on `enforce_auth_on_inference`
   (Settings → Client Settings, or `client_config` in the same call) so a
   request without a valid virtual key is refused rather than passed through.
   Then confirm the governance API refuses unauthenticated requests. The
   control plane authenticates with HTTP Basic — the admin `username:password`
   IS its `SQUELCH_CONTROL_BIFROST_ADMIN_TOKEN` (session bearer tokens expire
   monthly; do not paste one there). The exact first-boot flow is Bifrost's,
   not ours, and drifts with their releases; what must be true at the end is
   fixed: **the `/api/*` plane and the UI demand credentials, and
   `/anthropic` demands a virtual key.**
6. Bifrost auto-detects `ANTHROPIC_API_KEY` into a provider key on first
   boot, but two of its defaults do not survive contact with our models
   (v1.6.9): the key's `models: ["*"]` wildcard does not match models missing
   from Bifrost's own catalog, and routing resolves keys only through a
   virtual key's `provider_configs.key_ids`. The control plane handles the
   latter when it mints (it discovers key ids from
   `/api/providers/anthropic/keys`); the former is what
   `squelch-control llm sync` is for, and it is not a one-time setup step.
   Run it once here, and again after **every** change to the model the fleet
   runs. `squelch-control llm sync --check` reports and exits 1 without
   writing, so it can run as a check.

   **A model is allow-listed twice, and the second list is the one that gets
   forgotten.** A virtual key's `allowed_models` is matched against the id
   the daemon sent (`anthropic/claude-opus-5`). The provider key's `models`
   is matched *after* the provider prefix is resolved away
   (`claude-opus-5`). A model in the first and not the second answers **400
   `no keys found that support model: <model>`** — not `no keys found for
   provider`, which is the different failure of a virtual key with no
   `key_ids`.

   That gap is what took the hosted fleet down for four days in August 2026.
   `SQUELCH_CONTROL_LLM_MODELS`, the warden's stage models, and every virtual
   key had moved to `claude-opus-5`; this list still said `claude-opus-4-8`,
   so every tenant 400'd at routing and ran heuristics-only while every place
   an operator would look read as correct. `llm sync` exists so this list has
   an owner instead of a person who has to remember it.

   Two properties of this key are worth knowing before touching it by hand:

   - **Empty does not mean "allow everything."** Verified live on
     2026-08-25: emptying `models` leaves the key serving nothing at all,
     the same as the `["*"]` wildcard. It is an allow-list that has to name
     every model, which is why the answer is a sync command and not a
     deletion.
   - **A read masks the credential.** The key comes back with the Anthropic
     key as `sk-a****gQAA` plus a `ref` naming the env var holding the real
     one. The obvious read-modify-write therefore persists asterisks as your
     Anthropic key and takes every tenant down at once. `llm sync` sends the
     reference alone and refuses a key stored any other way; if you `PUT` it
     by hand, do the same.

   Verify with a curl through a test virtual key before pointing any tenant
   at it.

Then point both planes at it:

- Warden (`15-warden-config.yaml`): `SQUELCH_WARDEN_LLM_BASE_URL` =
  `https://<the-bifrost-domain>/anthropic`. This is the feature gate — with it
  unset the warden injects no LLM env at all and refuses llm-key installs, and
  it refuses to boot if any of the tuning knobs
  (`SQUELCH_WARDEN_LLM_STAGE1_MODEL`, `SQUELCH_WARDEN_LLM_STAGE2_MODEL`,
  `SQUELCH_WARDEN_LLM_STAGE1_DAILY_CAP`, `SQUELCH_WARDEN_LLM_STAGE2_DAILY_CAP`)
  are set without it.
- Control plane (Railway `control` service): the all-or-nothing trio
  `SQUELCH_CONTROL_BIFROST_URL` (the bare gateway origin, no `/anthropic`),
  `SQUELCH_CONTROL_BIFROST_ADMIN_TOKEN`, and `SQUELCH_CONTROL_LLM_BUDGET_USD`
  (monthly, per tenant). A partial trio refuses to boot: a control plane that
  silently stopped minting keys is an operator believing something false.

From then on every signup mints and installs its own key. The failure mode is
deliberately soft: if Bifrost is down mid-signup, the tenant still provisions
and triages on rules alone, the control plane logs the miss loudly, and

```sh
squelch-control llm mint <label>
```

backfills the keys later — the same command rotates them (it prints the old
Bifrost key ids; revoke them there) and keys tenants that predate the gateway.

Every mint is TWO virtual keys: the triage key and the assistant key
(`tenant-<label>-assistant`, its own budget and model list). The assistant key
never leaves the pod: the daemon holds it and proxies the Passband app's
assistant chats through `/client/assistant/messages`, so the app spends
against the tenant's assistant budget without ever seeing a credential. A
tenant minted before the assistant era keeps working; the app's relay option
simply does not appear until the tenant is re-minted.

## 7. Tenant limits

```sh
kubectl apply -f deploy/hosted/70-tenant-limits.yaml
```

A LimitRange and a ResourceQuota on `tenants`. **Size them to your box before
you apply them**; the shipped numbers assume the 2 vCPU / 4 GB floor above and
are deliberately conservative.

The warden already puts requests and limits on both containers of every tenant
pod (`SQUELCH_WARDEN_CPU_REQUEST` and friends in `15-warden-config.yaml`,
defaulting to 100m/256Mi requested and 1000m/1Gi allowed, plus a 512Mi cap on
the pod's `/tmp` and an ephemeral-storage bound so a runaway tenant cannot fill
the node's root filesystem). This file is the layer under that: defaults for
anything that lands in the namespace without bounds of its own, and an aggregate
ceiling.

Sizing, in the order that matters:

- **`requests.cpu` and `requests.memory` are your tenant count.** The scheduler
  reserves requests, so at the shipped 100m/256Mi a 2-vCPU box carries about 20
  tenants and no more, whatever the quota says about pods.
- **Limits may oversubscribe.** Tenants are idle most of the time and a sync
  burst is seconds long. 4x requests is comfortable on one node; much past that
  and a few simultaneous backfills evict each other.
- **`requests.storage` is disk you actually have.** local-path is a real
  filesystem — the root disk, or the block volume if you repointed it above — and
  it does not enforce a claim's size, so this quota is the only thing standing
  between N tenants and a full disk. Size it to whichever filesystem local-path
  is writing to, and a claim that cannot bind is a tenant that never comes up.
- Raising a per-tenant limit without raising the quota simply means fewer
  tenants. That is the correct trade to make consciously rather than discover.

A tenant refused by the quota looks like a provision that times out
(`500 not_ready`), with the real reason in
`kubectl -n tenants describe replicaset`.

## 8. The warden

Three edits, in two files:

- `15-warden-config.yaml` — the three places marked `EDIT ME`: your base domain,
  the squelchd image tenants run, and the control plane's origin. This ConfigMap
  is every knob the warden reads, and BOTH processes that render tenants read it
  through `envFrom` — the serving pod and the fleet roller. That is the whole
  reason it is a separate object: two env blocks that disagree render two
  different Deployments for the same tenant and take turns rewriting it, and a
  tenant image bumped in one and not the other silently pins the fleet backwards.
- `20-warden.yaml` — the warden's own `image:`, and its hostname in two spots on
  the Ingress.
- `90-warden-roller.yaml` — the same warden `image:` as `20-warden.yaml`, and
  nothing else. An image is a pod-spec field, so it is the one value the
  ConfigMap cannot hold for both; this binary is the renderer, and a roller on an
  older one renders older tenants.

```sh
kubectl apply -f deploy/hosted/10-warden-rbac.yaml
kubectl apply -f deploy/hosted/30-tenants-default-deny.yaml
kubectl apply -f deploy/hosted/15-warden-config.yaml
kubectl apply -f deploy/hosted/20-warden.yaml

# Check the API server's in-cluster address matches the policy before applying it.
kubectl -n default get svc kubernetes -o jsonpath='{.spec.clusterIP}'; echo
kubectl apply -f deploy/hosted/50-warden-networkpolicy.yaml

# The fleet roller: the same image on a timer, walking existing tenants onto the
# warden's current render one at a time. Nothing to converge yet on a fresh box,
# which is the cheapest possible first run. Same two CIDRs to check as above.
kubectl apply -f deploy/hosted/90-warden-roller.yaml

kubectl -n warden rollout status deploy/squelch-warden
```

The ConfigMap goes on before the two things that consume it. From then on, every
change to it is two commands rather than one:

```sh
kubectl apply -f deploy/hosted/15-warden-config.yaml
kubectl -n warden rollout restart deploy/squelch-warden
```

`envFrom` is read once, when a pod starts. The roller picks a change up on its
next tick because every run is a fresh pod; the serving warden does not, and goes
on rendering the old values into new signups until it is restarted. Same minute,
not same afternoon.

Confirm it is up and refuses strangers:

```sh
curl -sS https://warden.passband.app/healthz                                       # ok
curl -sS -o /dev/null -w '%{http_code}\n' https://warden.passband.app/v1/tenants   # 401
```

A 401 with an empty body is the correct answer to every wrong credential, every
missing header, and every malformed one. There is no other shape of failure on
that surface.

## 9. The first tenant, and verifying the policy

Normally the control plane does this at the end of a signup. To prove the
cluster works before wiring signup up, walk the two phases by hand:

```sh
export TOKEN=$(kubectl -n warden get secret squelch-warden -o jsonpath='{.data.token}' | base64 -d)
export W=https://warden.passband.app

# Phase one: mint this tenant's key. Nothing runs yet.
RECIPIENT=$(curl -sS -X POST $W/v1/tenants \
  -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  -d '{"label":"test1","account_email":"you@example.com"}' | jq -r .recipient)
echo "$RECIPIENT"          # age1...

curl -sS -H "authorization: Bearer $TOKEN" $W/v1/tenants/test1    # {"status":"pending"}

# Phase two: seal something to that recipient and hand it over.
CT=$(printf '{"slots":{}}' | age -a -r "$RECIPIENT")
curl -sS -X PUT $W/v1/tenants/test1/credentials \
  -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  -d "$(jq -n --arg ct "$CT" '{cred_read_ciphertext:$ct}')"
```

The `PUT` blocks while the pod pulls its image and comes up, then returns the
pairing code, the tenant URL, and the `passband://pair?...` deep link. Then:

```sh
kubectl -n tenants get all,pvc,netpol -l app.kubernetes.io/instance=test1
curl -sS -o /dev/null -w '%{http_code}\n' https://test1.passband.email/client/updates  # 401
curl -sS -o /dev/null -w '%{http_code}\n' https://test1.passband.email/mcp             # 404
```

Those last two lines are the two-door split, observable from the internet: the
human door answers (with a 401, because you presented no token) and the agent
door does not exist. See "The agent door is not served" below.

The daemon will fail to sync with that empty credential, which is expected. What
this proves is the image, the volume, the certificate, the routing, and the
pairing exec.

### Do the probes get through?

**Do this with the canary above, before you have real tenants.** A readiness
probe is a TCP connection from the KUBELET, which originates at the node's own
address and matches no pod and no namespace, so it matches no peer in a tenant's
NetworkPolicy. Some CNIs exempt host-originated traffic from policy and some do
not, and on one that does not, every provision times out on a pod that is
perfectly healthy. k3s ships kube-router, and this is worth ten minutes of
certainty rather than an assumption.

```sh
kubectl -n tenants get pod -l app.kubernetes.io/instance=test1 \
  -o custom-columns=NAME:.metadata.name,READY:.status.containerStatuses[0].ready
kubectl -n tenants describe pod -l app.kubernetes.io/instance=test1 | grep -A3 Events
```

If the `PUT` returned a pairing code, probes are getting through and there is
nothing to do. If it returned `500 not_ready` while the container itself is
running and the events show readiness probe timeouts, the probe is being
dropped. Set the node network on the warden and re-apply:

```sh
kubectl get node -o jsonpath='{.items[*].status.addresses[?(@.type=="InternalIP")].address}'; echo
# put that address as a /32 in SQUELCH_WARDEN_NODE_CIDR in 15-warden-config.yaml
kubectl apply -f deploy/hosted/15-warden-config.yaml
kubectl -n warden rollout restart deploy/squelch-warden
```

Then delete and re-provision the canary. The warden adds one ingress rule to
every tenant's policy: that CIDR, TCP 8848 and TCP 9464, nothing else. Both
ports whichever probe is configured, because 9464 is where the kubelet lands
once `SQUELCH_WARDEN_HTTP_READINESS` is on and a policy is precisely what no
roll can backfill — see the warning immediately below.

**Do this before you have real tenants, because this one does not backfill.**
The rule lands on a tenant's NetworkPolicy, and the fleet roller only ever looks
at Deployments — so no roll will report an existing tenant as drifted for it, and
no roll will deliver it. Tenants that already exist need
`squelch-control reconcile <label>` each, which re-applies all five of their
objects. Same for `SQUELCH_WARDEN_TLS_SECRET`, the ingress settings and
`SQUELCH_WARDEN_STORAGE_*`.

The tradeoff, stated: it admits traffic originating at the node's address, which
is the kubelet and anything else with a shell on the node. Anything with a shell
on the node can read every volume on the box anyway, so this opens no new door.
Other tenants' pods arrive from pod IPs, not the node IP, and stay blocked.
Resist widening it to the pod CIDR: that is exactly the tenant-to-tenant
reachability the rest of this design spends its effort removing. The warden
refuses a `/0` outright.

### Does the policy actually deny?

The claim is that nothing reaches a tenant except the ingress controller. Check
it rather than believing the YAML, with a scratch pod in the tenant namespace
aimed at the canary's Service:

```sh
kubectl -n tenants run probe --rm -it --restart=Never --image=busybox:1.36 \
  --overrides='{"spec":{"automountServiceAccountToken":false,"securityContext":{"runAsNonRoot":true,"runAsUser":10001,"runAsGroup":10001,"seccompProfile":{"type":"RuntimeDefault"}},"containers":[{"name":"probe","image":"busybox:1.36","command":["sh","-c","nc -w 3 test1 8848 </dev/null && echo REACHABLE || echo blocked"],"securityContext":{"allowPrivilegeEscalation":false,"readOnlyRootFilesystem":true,"capabilities":{"drop":["ALL"]}}}]}}'
```

`blocked` is the correct answer, and it is the whole tenant-isolation claim in
one line: a pod in the same namespace, on the same node, cannot open a
connection to another tenant's daemon. `REACHABLE` means the CNI is not
enforcing NetworkPolicy at all (a cluster installed with
`--disable-network-policy`, or a CNI that ignores it), and **nothing else here is
worth doing until that is fixed**: per-tenant isolation is the product.

The overrides are there because the namespace enforces Pod Security Admission at
`restricted`, so a bare `kubectl run` is refused. That refusal is itself the
admission control working, and a second thing this line demonstrates.

Clean up:

```sh
curl -sS -X DELETE -H "authorization: Bearer $TOKEN" $W/v1/tenants/test1   # 204
```

`DELETE` removes the Deployment, Service, Ingress and NetworkPolicy. It KEEPS
the volume and both Secrets, and the warden has no permission to remove them, so
nothing reachable from the control plane can destroy a mailbox. Removing a test
tenant for real is you, with kubectl:

```sh
kubectl -n tenants delete pvc test1-data secret/test1-identity secret/test1-credential
```

## 10. Embedding weights

Each tenant's daemon downloads about 126 MB of ONNX weights the first time it
embeds a message, into `$HOME/.local/share/squelch/models`, and `HOME` is that
tenant's own volume. Left alone, that is the same download once per tenant,
inside a signup somebody is watching.

It is worse than slow. The daemon binds its listeners and starts its 30-day
initial mail backfill while the embedder is still fetching weights, so on a cold
cache the backfill outruns the embedder and thousands of messages land with no
vectors: search is keyword-only until the vector backfill catches up, on a
mailbox whose owner just signed up. It is also the prerequisite for
`SQUELCH_WARDEN_HTTP_READINESS`, which puts that download inside every readiness
wait the warden makes (see PRODUCTION.md, "Turning on the HTTP readiness probe").

**The chosen mechanism: one shared read-only volume, copied into each tenant's
volume by its init container.** The warden mounts the shared PVC into the init
container only, which copies across any model directory the tenant does not
already have. It is a copy rather than a symlink because the daemon's root
filesystem is read-only and fastembed expects to own its cache directory; the
cost is ~126 MB of local disk per tenant, which is nothing next to a mail index.

### The exact layout the init container expects

The PVC is mounted at `/models`, and the init container copies **each top-level
entry of `/models`** to `/data/.local/share/squelch/models/<same name>`, skipping
any that is already there. So the root of the volume is the cache directory: one
directory per model, no wrapper.

The seed pod in `60-models.yaml` mounts the same PVC at `/seed`, which means what
you are building is:

```
/seed/models--Xenova--bge-small-en-v1.5/
├── blobs/                      # the real files, content-addressed
├── refs/main
└── snapshots/<sha>/
    ├── config.json
    ├── tokenizer.json
    └── onnx/model.onnx         # a SYMLINK into ../../../blobs/
```

That is fastembed's Hugging Face cache layout, and the symlink line is the part
that bites. The files under `snapshots/` are relative symlinks into `blobs/`;
they resolve correctly only if `blobs/` came along with them.

**Seed the Xenova directory and only that one.** `Xenova/bge-small-en-v1.5` is
the pinned model. A box that has been running a while also has
`models--Qdrant--bge-small-en-v1.5-onnx-Q` sitting beside it, 63 MB of a
quantized build from back when the model choice was resolved by substring match
and came out nondeterministically. It is dead weight. Copying it into the shared
volume would hand every future tenant a copy of it too, forever.

### Fill it once

Two sources, in preference order. Both end at the same shape.

**From a tenant already running on this box** (nothing to download, and the
weights are known-good because a daemon loaded them):

```sh
kubectl apply -f deploy/hosted/60-models.yaml
kubectl -n tenants wait --for=condition=Ready pod/squelch-models-seed

POD=$(kubectl -n tenants get pod -l app.kubernetes.io/instance=<label> -o name | head -1)
kubectl -n tenants exec "${POD#pod/}" -c squelchd -- \
  tar -C /data/.local/share/squelch/models -cf - models--Xenova--bge-small-en-v1.5 \
| kubectl -n tenants exec -i squelch-models-seed -- tar -C /seed -xf -
```

**A tar stream through `exec` rather than `kubectl cp`, on purpose.** `kubectl
cp` is a tar stream too, but it owns both ends of it, and which kubectl versions
recreate a symlink, dereference it, or refuse the entry outright has changed more
than once. Driving `tar` yourself is the same transfer with the behaviour
written down: GNU tar stores a symlink as a symlink at both ends, so the tree
lands byte-identical. If your tar does refuse, `tar -ch` on the sending side
dereferences instead, which also works and costs ~63 MB of duplicated blobs on
the volume; fastembed does not care which it gets.

**From a laptop**, if there is no tenant to copy from yet. Run `squelchd` once
against any mailbox, let it print `embedder ready`, then send the same stream:

```sh
tar -C ~/.local/share/squelch/models -cf - models--Xenova--bge-small-en-v1.5 \
| kubectl -n tenants exec -i squelch-models-seed -- tar -C /seed -xf -
```

If the extract fails with `Permission denied`, the volume directory on the node
is not writable by uid 10001. `local-path` creates it `0777` and normally is,
and `fsGroup` does not help here because the kubelet does not manage ownership
on host-path-backed volumes. Fix it on the box (`chown -R 10001:10001` under
`/var/lib/rancher/k3s/storage/<pv>`), not by making the seed pod root: the
`tenants` namespace enforces Pod Security Admission at `restricted` and will
refuse a root pod outright.

### Verify before you point the warden at it

A `SQUELCH_WARDEN_MODEL_PVC` naming an empty volume is not a slow signup, it is
every tenant with a cache directory that exists and has nothing in it. Check the
volume first:

```sh
kubectl -n tenants exec squelch-models-seed -- ls /seed
# models--Xenova--bge-small-en-v1.5      <- this, and nothing else

# The symlinks resolve and the blob is really there: -L dereferences, so a
# broken link is an error rather than a plausible-looking 60-byte listing.
kubectl -n tenants exec squelch-models-seed -- \
  find /seed -name '*.onnx' -exec ls -lL {} \;
# ... 126 MB or so for model.onnx

kubectl -n tenants exec squelch-models-seed -- du -sh /seed
```

Then delete the seed pod: `kubectl -n tenants delete pod squelch-models-seed`.
Nothing references it afterwards, and leaving it running holds a
`ReadWriteOnce` volume open for an hour for no reason.

### Turn it on

`SQUELCH_WARDEN_MODEL_PVC: "squelch-models"` is set in `15-warden-config.yaml`.
Apply it and restart the warden:

```sh
kubectl apply -f deploy/hosted/15-warden-config.yaml
kubectl -n warden rollout restart deploy/squelch-warden
```

The mount is part of a tenant's pod spec, so the roller reads it as drift and
every existing tenant takes one pod restart for it within a tick or two. That is
expected, and worth knowing before you watch the fleet cycle.

Verify on the next tenant that signs up. Its init container copies from a local
disk in a couple of seconds instead of pulling from Hugging Face, and its log
should reach `embedder ready` with no download line before it:

```sh
kubectl -n tenants logs deploy/<label> -c squelchd | grep -E 'embedding model|embedder ready'
# squelchd: embedder ready — semantic + hybrid search now enabled
```

A `squelch: downloading embedding model ... (first run only)` line above it means
the seeded directory is not the model the daemon resolved. Compare the name the
daemon logs at init against the directory in `/seed`; a mismatch there is the
whole failure mode, and it is silent otherwise (the tenant works, it just pays
the download).

Existing tenants keep whatever they already have. Their cache directory is
already populated, and the init container only fills in entries that are missing,
so seeding changes nothing for the tenants provisioned before the volume existed.
That is correct: they have already paid the download, and the copy on their own
volume is the same weights.

The `ReadWriteOnce` volume is mounted by many pods, which is legal because they
are all on the one node. **If you ever add a second node, this stops working.**
The answer then is the other mechanism: build a squelchd image with the weights
baked in at `/data/.local/share/squelch/models` and point
`SQUELCH_WARDEN_IMAGE` at it. That costs image size and gains node
independence.

## 11. Backups: Litestream to R2

Everything above this line builds a box where losing one disk loses every
mailbox on it. This section is the answer, and it is deliberately the last thing
you install: it wants tenants to exist so you can watch it pick one up.

Litestream tails each tenant's SQLite write-ahead log and streams it to
Cloudflare R2 continuously, **age-encrypted under a keypair we hold**. The
recovery objective it buys is **minutes**, not a nightly snapshot's worth of
hours, and it costs one process and pennies of storage. Artifacts live in
`deploy/hosted/litestream/`.

**Pinned to litestream 0.3.13, and that is a decision, not staleness.** 0.3.x is
the last line with client-side age encryption; 0.5.x removed it and refuses to
start on a config that asks for it. Encrypting the copy we hand to a third party
is worth more here than being on the maintained line, because the thing being
replicated is the full text of other people's mail. The cost, stated: 0.3.13
shipped in October 2023 and receives no fixes. The mitigation is running the
restore drill in step 6 on a schedule, not after a disaster.

The two lines also disagree about config schema in a way that **fails silently** —
0.3 wants `replicas:` (a list), 0.5 wants `replica:` (one mapping), and 0.3.13
accepts a 0.5-shaped config without complaint and attaches zero replicas. If you
ever hand-edit `/etc/litestream.yml`, `litestream databases` must still print
`s3` in the replicas column. `deploy/hosted/litestream/README.md` has the full
table.

**One process on the HOST, not a sidecar in each tenant pod.** This is the
design decision worth understanding before you install anything, because the
sidecar is the obvious shape and it is wrong here. A sidecar needs the R2 write
credential *inside the tenant pod*. S3-style credentials cannot be meaningfully
scoped to "your own prefix, append only", so every tenant would hold a key that
can delete every other tenant's backups, and one tenant getting code execution
in their own pod would become a fleet-wide loss of the exact thing backups exist
for. Host-level keeps that credential with root, which already owns the disk all
of these databases sit on — it hands a tenant pod nothing it did not have. The
price is that litestream must discover tenants itself, which is what the config
generator in step 4 is for. `deploy/hosted/litestream/README.md` has the full
ledger, including the costs.

### 1. Install litestream

The Debian package, from the upstream release. **Do not substitute a newer
tag** — read the pin note above first; 0.5.x cannot do what this section is for.
Upstream ships a `.deb` for `amd64` and `arm64` at v0.3.13, so this is a package
install, not a tarball.

```sh
LS=v0.3.13
A=$(dpkg --print-architecture)      # amd64 or arm64; note 0.3.x uses dpkg's
                                    # names, not the x86_64 the 0.5 assets use
case "$A" in amd64|arm64) ;; *) echo "no 0.3.13 .deb for $A"; exit 1;; esac
wget "https://github.com/benbjohnson/litestream/releases/download/${LS}/litestream-${LS}-linux-${A}.deb"
dpkg -i "litestream-${LS}-linux-${A}.deb"
litestream version     # v0.3.13
```

Then hold it, because an unattended upgrade to the 0.5 line would take the
encryption away and, worse, leave a config that parses:

```sh
apt-mark hold litestream
```

The package installs `/usr/bin/litestream`, a four-line systemd unit at
`/usr/lib/systemd/system/litestream.service`, and a placeholder
`/etc/litestream.yml`. We replace the first two of those; the third gets
overwritten by the generator. Do **not** `systemctl enable litestream` yet.

You also need `age`, for the keypair in step 3:

```sh
apt install age
```

### 2. The R2 bucket and a bucket-scoped token

In the Cloudflare dashboard, on the account that owns `passband.email`:

1. **R2 → Create bucket.** Name it `passband-tenant-backups`. Location: pick the
   automatic hint nearest the box. There is no reason to make it public and
   several not to; leave public access off.
2. **Object lifecycle:** leave it alone. Litestream manages its own retention
   and a lifecycle rule that expires objects underneath it will quietly eat the
   history you are paying for.
3. **R2 → API → Manage API Tokens → Create API Token.**
   - Permission: **Object Read & Write**. Not Admin.
   - Specify bucket: **`passband-tenant-backups` only**. This credential is
     about to live on an internet-facing box; the blast radius of it leaking
     should be one bucket, not all of R2.
   - TTL: whatever your rotation appetite is. Expiry here is silent replication
     failure later, so if you set one, set a calendar reminder with it.
4. Cloudflare shows the **Access Key ID**, the **Secret Access Key** and the
   **S3 endpoint** (`<32-hex-account-id>.r2.cloudflarestorage.com`) exactly
   once. Take all three.

Every tenant is a prefix inside this one bucket
(`tenants/<label>/store.db`), so there is nothing to create per signup.

### 3. The encryption keypair

One keypair for the whole fleet's backups. Litestream seals every snapshot and
every WAL segment to it before upload, so what lands in R2 begins
`age-encryption.org/v1` and Cloudflare holds ciphertext.

```sh
umask 077
install -d -m 0700 -o root -g root /etc/litestream
age-keygen -o /etc/litestream/backup-age.key
# Public key: age1...        <- this is the RECIPIENT. Copy it.
chmod 0600 /etc/litestream/backup-age.key
chown root:root /etc/litestream/backup-age.key
```

`age-keygen` prints the public key and also writes it into the file's header
comment; `age-keygen -y /etc/litestream/backup-age.key` re-derives it any time.

The split that makes this worth doing:

| | Where it lives | Who uses it |
|---|---|---|
| **recipient** (public) | `LITESTREAM_AGE_RECIPIENT` in `/etc/litestream/env` | the always-running replicator, to seal |
| **identity** (private) | `/etc/litestream/backup-age.key`, root 0600, read by nothing automatically | a human, exported into one shell, for the minutes a restore takes |

The replicating daemon therefore **cannot decrypt a single backup it writes**.
That is deliberate, and it is why the identity is not in `/etc/litestream/env`
next to the R2 keys.

> ## ⚠ THE IDENTITY IS THE BACKUP. PUT IT IN THE PASSWORD MANAGER NOW.
>
> **Lose `/etc/litestream/backup-age.key` and every byte in R2 becomes
> permanently unreadable — by us, by Cloudflare, by anyone. There is no escrow
> and there is no recovery.**
>
> Do it before you start replicating, not after: a backup key that exists only
> on the machine it protects is not a backup key, it is a second copy of the
> same failure. Store it beside your tenant identity Secrets dump. Those two
> things are the entirety of what a re-sync from Gmail cannot rebuild.
>
> If it is ever exposed: mint a new pair, change `LITESTREAM_AGE_RECIPIENT`,
> restart litestream — and **keep the old identity forever**, because every
> object written before the swap is still sealed to it.

#### What the R2 copy is and is not protected by

**Is:** age encryption with a key that never leaves this box and your password
manager (X25519 + ChaCha20-Poly1305, client-side, before upload); R2 encryption
at rest underneath that; TLS in transit; and a token scoped to one bucket.

**Is not:** protection against losing that key — see the warning above — and not
protection against root on `carrier`, which can read every tenant's live
database directly and does not need the backups at all (see "Root on the node
reads everything").

What is *in* these backups, since encryption is not a reason to stop caring:

| Thing | In the R2 backup? |
|---|---|
| a tenant's mail index, bodies, subjects, contacts | **yes** — this is the whole point, and it is ciphertext at rest |
| a tenant's age identity (`<label>-identity`) | no. Kubernetes Secret, k3s datastore, never on this volume |
| a tenant's sealed Google credential (`credentials.json`) | no. It is on the volume but it is not SQLite, so litestream does not touch it — and it is age ciphertext anyway |
| the Google OAuth client secret | no |
| the backup age identity itself | **no, and never** — do not "back up the backup key" into the bucket it opens |

So a compromised R2 bucket, on its own, is not a disclosure at all: it is a pile
of age files. A compromised bucket **plus** the backup identity is a mail-content
disclosure, and still not a path to anyone's mailbox, Google account, or
credential.

### 4. Wire it up

```sh
cd /path/to/squelch/deploy/hosted/litestream

# The credential. 0600, root, and nothing else on the box reads it.
install -d -m 0700 -o root -g root /etc/litestream
install -m 0600 -o root -g root env.example /etc/litestream/env
${EDITOR:-vi} /etc/litestream/env        # four values from step 2, plus the
                                         # age RECIPIENT from step 3

# Litestream's own state, off the tenant volume on purpose (see below).
install -d -m 0700 -o root -g root /var/lib/litestream

# The generator, and the units.
install -m 0755 -o root -g root litestream-sync-config.sh /usr/local/bin/
install -m 0644 -o root -g root litestream.service litestream-config.service \
  litestream-config.timer /etc/systemd/system/
systemctl daemon-reload

# Our unit outranks the .deb's -- confirm it, don't assume it.
systemctl cat litestream.service | head -1     # /etc/systemd/system/litestream.service
```

**Check the tenant root before you go further.** `TENANT_ROOT` at the top of
`litestream-sync-config.sh`, and `RequiresMountsFor=` in both units, default to
`/mnt/tenant-data` — the block volume from "Tenant data on a block volume"
above. If you never repointed local-path, all three are
`/var/lib/rancher/k3s/storage`. A wrong value here is a service that runs
happily and backs up nothing.

Then look before you leap:

```sh
DRY_RUN=1 VERBOSE=1 /usr/local/bin/litestream-sync-config.sh
```

That prints the config it *would* write and changes nothing. You should see one
`- path:` block per tenant that has synced at least once, each with a
`meta-path` under `/var/lib/litestream/`, a `replicas:` **list** whose single
entry has a path of `tenants/<label>/store.db`, and an `age: recipients:` block.
If you see zero blocks and you have tenants, the tenant root is wrong. If you
see credentials or an `age1...` string in that output, stop and file a bug: the
config is supposed to carry `${LITESTREAM_R2_*}` and
`${LITESTREAM_AGE_RECIPIENT}` names only, which is the entire reason it is safe
to read.

The generator refuses, loudly and with the fix, if `/etc/litestream/env` is
missing a value, is looser than 0600 root, or spells `LITESTREAM_R2_ENDPOINT`
with an `https://` scheme. That last one is a house rule rather than litestream's
— it accepts either — but everything downstream here composes
`https://${LITESTREAM_R2_ENDPOINT}`, and `https://https://` is a miserable thing
to debug mid-restore.

**If you are moving an existing box onto these artifacts,** rename the variables
in `/etc/litestream/env` to the canonical set. Litestream 0.3.13 reads exactly
one variable on its own (`LITESTREAM_CONFIG`); every other name matters only
because the generated config spells it, so older names like
`LITESTREAM_ACCESS_KEY_ID`, `R2_ENDPOINT` and `R2_BUCKET` are read by nothing:

| Old | Canonical |
|---|---|
| `LITESTREAM_ACCESS_KEY_ID` | `LITESTREAM_R2_ACCESS_KEY_ID` |
| `LITESTREAM_SECRET_ACCESS_KEY` | `LITESTREAM_R2_SECRET_ACCESS_KEY` |
| `R2_ENDPOINT` (with `https://`) | `LITESTREAM_R2_ENDPOINT` (bare host) |
| `R2_BUCKET` | `LITESTREAM_R2_BUCKET` |
| — | `LITESTREAM_AGE_RECIPIENT` (new, required) |

Happy? Start it:

```sh
systemctl enable --now litestream-config.timer
systemctl start litestream-config          # don't wait 2 minutes for the first tick
systemctl enable --now litestream
systemctl status litestream litestream-config.timer
```

Order matters slightly: the timer renders the config, and litestream started
against the .deb's placeholder simply logs "no databases specified in
configuration" and idles until the next render. It does not exit, so there is no
crash loop either way.

**Why litestream's state lives in `/var/lib/litestream/<label>`.** Its default
is a hidden `.squelch.db-litestream` directory beside the database — which is
*inside the volume the tenant's own pod mounts read-write*. Relocating it keeps
the bookkeeping that describes each backup out of tenant-writable space. The
generator emits `meta-path` for every database to do this; it is not optional
polish.

### 5. Verify it is actually replicating

Four checks, cheapest first. The last one is the only one that proves bytes left
the building.

```sh
set -a; . /etc/litestream/env; set +a     # the CLI needs the same credentials

litestream databases      # every tenant listed, and "s3" in the replicas column
```

**Read the replicas column, not just the paths.** A blank there is the silent
0.5-schema failure: databases understood, nothing replicated. There is no
`litestream status` on 0.3.x; local progress comes from the journal
(`journalctl -u litestream`, one "wal segment written" line per sync).

For what actually reached R2 — 0.3.x calls these `generations`, `snapshots` and
`wal`, not `ltx`:

```sh
DB=/mnt/tenant-data/pvc-<uid>_tenants_<label>-data/squelch.db
litestream generations "$DB"    # generation, lag, and the time range it covers
litestream snapshots   "$DB"
```

A generation with a small lag is replication working. These read object metadata
only, so they need the R2 credentials but **not** the age identity. Then the
ground truth, from outside litestream entirely:

```sh
aws --endpoint-url "https://${LITESTREAM_R2_ENDPOINT}" \
    s3 ls "s3://${LITESTREAM_R2_BUCKET}/tenants/" --recursive | head
```

You are looking for `tenants/<label>/store.db/generations/<gen>/snapshots/...`
and `.../wal/...`. (`AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` set to the R2
pair, `AWS_DEFAULT_REGION=auto`. `rclone` works equally well. This is worth doing
once so you know what a healthy bucket looks like when you are staring at an
unhealthy one.)

While you are in there, confirm the encryption once, with your own eyes:

```sh
aws --endpoint-url "https://${LITESTREAM_R2_ENDPOINT}" \
    s3 cp "s3://${LITESTREAM_R2_BUCKET}/tenants/<label>/store.db/generations/<gen>/snapshots/00000000.snapshot.lz4" - \
  | head -c 21
# age-encryption.org/v1
```

If that prints anything else, the backups are not sealed and something is wrong
with `LITESTREAM_AGE_RECIPIENT`. Do not proceed on the assumption that it is.

And prove the discovery loop, which is the part with a moving piece: provision a
canary tenant as in §9, wait two minutes, and confirm it appears in
`litestream databases` on its own. A tenant that never shows up is the whole
mechanism failing silently.

```sh
journalctl -u litestream-config --since -10min    # what the generator decided
journalctl -u litestream -f                       # replication, live
```

### 6. Restore drill: one tenant, nothing stopped

Do this now, on the canary, while nothing is on fire. A backup you have never
restored is a hypothesis.

**A restore needs the age identity, and `/etc/litestream.yml` does not have it.**
That is the design (step 3), so every restore starts with two extra lines: load
the identity into this shell, and render a config that references it.

```sh
set -a; . /etc/litestream/env; set +a
export LITESTREAM_AGE_IDENTITY="$(grep -v '^#' /etc/litestream/backup-age.key | tr -d '[:space:]')"

umask 077
DRY_RUN=1 WITH_IDENTITY=1 /usr/local/bin/litestream-sync-config.sh > /root/restore.yml
```

`/root/restore.yml` is the same tenants and the same replicas as the live config,
plus an `identities:` line. It still contains no key material — only the
`${LITESTREAM_AGE_IDENTITY}` name — so the secret exists in that shell's
environment and nowhere on disk but the key file.

Restoring to a **scratch path** then touches no tenant and needs no downtime:

```sh
DB=/mnt/tenant-data/pvc-<uid>_tenants_<label>-data/squelch.db
litestream restore -config /root/restore.yml -o /root/restore-<label>.db "$DB"

sqlite3 /root/restore-<label>.db 'pragma integrity_check;'
sqlite3 /root/restore-<label>.db 'select count(*) from messages;'
```

The argument is the **live database path**, which litestream resolves through
the config to find the replica; `-o` sends the output somewhere else.
Point-in-time works the same way:

```sh
litestream restore -config /root/restore.yml -timestamp 2026-08-10T09:00:00Z \
  -o /root/rewind.db "$DB"
```

(There is no `-dry-run` on 0.3.x. To see what is available without fetching it,
use `litestream generations "$DB"` and `litestream snapshots "$DB"` from step 5.)

> **The error you will actually hit**, if you skip the identity or run in a
> different shell than the `export`:
>
> ```
> cannot restore snapshot: lz4: bad magic number
> ```
>
> That is litestream trying to decompress age ciphertext. It means "no identity",
> not "corrupt backup". Check `[ -n "$LITESTREAM_AGE_IDENTITY" ]` and that you
> passed `-config /root/restore.yml`.

To put a restored database **back** into a live tenant, stop **both** writers
first. There are two, and forgetting the second is the mistake worth naming: the
tenant's own daemon, and litestream itself, which is a SQLite client that
checkpoints this file. Swapping the database out from under a running litestream
leaves its bookkeeping describing a file that no longer exists.

```sh
# 1. The daemon.
kubectl -n tenants scale deploy/<label> --replicas=0
kubectl -n tenants wait --for=delete pod -l app.kubernetes.io/instance=<label> --timeout=120s

# 2. The replicator. Stop the timer too, or it restarts the service under you.
systemctl stop litestream-config.timer litestream

# 3. Stale -wal/-shm next to a restored database is its own corruption. Move the
#    whole set aside; do not delete until the tenant is verified up.
mkdir -p /root/old-<label>
mv "$DB" "$DB"-wal "$DB"-shm /root/old-<label>/ 2>/dev/null || true

# Same restore config as above -- the identity is what makes this possible.
litestream restore -config /root/restore.yml -o "$DB" "$DB"
chown --reference="$(dirname "$DB")" "$DB"      # see the note below

# 4. Litestream's local state for this tenant describes the database you just
#    replaced. Clear it and let it re-derive from the replica.
rm -rf /var/lib/litestream/<label>

# 5. Back up, replicator first so the daemon's first writes are covered.
systemctl start litestream litestream-config.timer
kubectl -n tenants scale deploy/<label> --replicas=1
litestream databases | grep <label>         # back in the config, with an s3 replica
journalctl -u litestream --since -2min | grep <label>   # and actually syncing

# 6. Done: drop the restore config and the identity out of this shell.
rm -f /root/restore.yml
unset LITESTREAM_AGE_IDENTITY
```

That `chown --reference` line is the one people forget, and it is the single
most likely way this drill ends in a tenant that will not come up.

**Why `--reference` and not a literal uid.** Tenant pods run as uid 10001 with
`fsGroup: 10001` and `fsGroupChangePolicy: OnRootMismatch`, which means the
kubelet only re-chowns a volume when its *root directory* looks wrong. Restore a
root-owned file into a directory that already looks right and nothing fixes it:
the pod starts, cannot write, and the failure surfaces as a sync error rather
than a permissions one. And with user namespaces on (`UserNamespacesSupport`,
§2), the on-disk owner is a *shifted* uid, not 10001 at all — so hard-coding
10001 is wrong on exactly the boxes this runbook tells you to build. Copying the
ownership off the directory that is already there is correct in both worlds.

### 7. Restore drill: the whole box is gone

The full-DR order, and the first step is the one that matters:

> **MASK LITESTREAM BEFORE YOU PROVISION ANYTHING.** On a fresh box, a
> re-provisioned tenant's daemon creates an empty `squelch.db` within seconds.
> If litestream is running, it starts replicating that empty database to the
> same R2 key holding the tenant's real history. Do not find out how gracefully
> it handles that. Mask, restore, then unmask.

```sh
# 0. Fresh box: §0 through §8 of this runbook, plus step 1 above (install the
#    litestream .deb and the units) -- but ENABLE NOTHING.
systemctl mask litestream.service litestream-config.timer

# 1. Restore the irreplaceable half FIRST, from the password manager and your
#    off-box dump: the BACKUP AGE IDENTITY and the tenant identity Secrets.
#    Without the first, every object in R2 is noise. Without the second, the
#    credentials are sealed to keys nobody has. Neither has an escrow.
umask 077
install -d -m 0700 -o root -g root /etc/litestream
${EDITOR:-vi} /etc/litestream/backup-age.key      # paste it back, 0600 root:root
chmod 0600 /etc/litestream/backup-age.key
age-keygen -y /etc/litestream/backup-age.key      # sanity: prints the recipient
kubectl apply -f tenant-secrets.yaml

# 2. Which tenants were there? The bucket is the inventory that survived.
set -a; . /etc/litestream/env; set +a
export LITESTREAM_AGE_IDENTITY="$(grep -v '^#' /etc/litestream/backup-age.key | tr -d '[:space:]')"
aws --endpoint-url "https://${LITESTREAM_R2_ENDPOINT}" \
    s3 ls "s3://${LITESTREAM_R2_BUCKET}/tenants/"

# 3. Per tenant: provision it (§9's two calls, or let the control plane do it),
#    which is what makes k3s local-path create the PVC directory at all --
#    the storage class binds on first consumer, so no pod means no directory.
#    Then immediately take the writer away again.
kubectl -n tenants scale deploy/<label> --replicas=0
kubectl -n tenants wait --for=delete pod -l app.kubernetes.io/instance=<label> --timeout=120s

DIR=$(ls -d /mnt/tenant-data/pvc-*_tenants_<label>-data)
rm -f "$DIR"/squelch.db "$DIR"/squelch.db-wal "$DIR"/squelch.db-shm

# 4. Restore. The generator cannot help here -- it only renders tenants whose
#    squelch.db already exists, and you just deleted it -- so hand-write the
#    scratch config. Note the 0.3 schema: replicas is a LIST, and age carries
#    the identities without which this downloads ciphertext and fails.
umask 077
cat > /tmp/restore.yml <<EOF
dbs:
  - path: ${DIR}/squelch.db
    replicas:
      - type: s3
        bucket: \${LITESTREAM_R2_BUCKET}
        path: tenants/<label>/store.db
        endpoint: \${LITESTREAM_R2_ENDPOINT}
        region: auto
        access-key-id: \${LITESTREAM_R2_ACCESS_KEY_ID}
        secret-access-key: \${LITESTREAM_R2_SECRET_ACCESS_KEY}
        age:
          identities:
            - \${LITESTREAM_AGE_IDENTITY}
          recipients:
            - \${LITESTREAM_AGE_RECIPIENT}
EOF
litestream restore -config /tmp/restore.yml "${DIR}/squelch.db"
chown --reference="$DIR" "${DIR}/squelch.db"    # see the note below
sqlite3 "${DIR}/squelch.db" 'pragma integrity_check;'

kubectl -n tenants scale deploy/<label> --replicas=1

# 5. Only once EVERY tenant is restored and verified:
rm -f /tmp/restore.yml
unset LITESTREAM_AGE_IDENTITY
systemctl unmask litestream.service litestream-config.timer
systemctl enable --now litestream-config.timer litestream
systemctl start litestream-config
litestream databases       # every restored tenant, WITH an s3 replica listed
```

(`lz4: bad magic number` at step 4 means the identity did not reach litestream:
either `LITESTREAM_AGE_IDENTITY` is empty in this shell, or the `age:` block is
missing from the scratch config.)

A tenant you cannot restore is not a disaster on its own: with a working
identity Secret and credential, that tenant re-syncs from Gmail. Slow and noisy,
but recoverable. A tenant whose **identity Secret** is gone is not recoverable by
anyone, us included — and if the **backup age identity** is gone, R2 is a pile of
noise for every tenant at once. Those two keys are step 1 for that reason, and
why the next section still says what it says.

### What this section was verified against

Litestream **v0.3.13**, on 2026-08-11. The config schema, the `age` block, the
CLI surface and the failure messages quoted above were read out of the v0.3.13
source and then confirmed by running the real v0.3.13 binary against a scratch
tenant layout: the generator's render is accepted (`litestream databases` reports
an `s3` replica per tenant), a replicate → restore round trip comes back with
`pragma integrity_check` = `ok`, the on-disk snapshot begins
`age-encryption.org/v1`, and restoring without `identities:` fails with exactly
`lz4: bad magic number`. The `.deb` asset names and layout above come from the
v0.3.13 release, not from memory.

Not verified, and worth ten seconds on the box the first time you install:
`systemd-analyze verify /etc/systemd/system/litestream*.{service,timer}`. The
units were written on a machine with no systemd.

## Operating notes

**Where the state is.** There is none. The warden keeps no database, no state
file and no port allocator; the cluster is the record. A tenant's status is
derived from what exists:

| What exists | Status |
|---|---|
| identity Secret only | `pending` (phase one done, phase two never ran) |
| Deployment with a ready replica | `active` |
| Deployment with no ready replica | `failed` |
| credential Secret, no Deployment | `stopped` (someone called DELETE) |
| no identity Secret | 404 |

**A signup that died between the two calls.** It shows as `pending`. Re-posting
the same label with the same address returns the SAME recipient rather than
minting a second key, so the control plane's retry is safe and idempotent. A
different address on a pending label is a 409: that is two people asking for one
subdomain, not a retry.

A pending tenant nobody ever comes back for is collected automatically once it
is older than `SQUELCH_WARDEN_PENDING_TTL_SECS` (24 hours by default): the
warden deletes its identity Secret, which frees the subdomain. Nothing is lost,
because nothing was ever sealed to that key and no mail exists; recovery for the
person is signing up again. The sweep only ever touches a tenant that is still
`pending` at the moment it looks, and only one whose Secret it stamped with a
creation time, so a serving tenant, a stopped one, and a Secret restored from a
backup are all beyond it.

**A tenant stuck in `failed`.** Look at the pod, not the warden:

```sh
kubectl -n tenants describe pod -l app.kubernetes.io/instance=<label>
kubectl -n tenants logs -l app.kubernetes.io/instance=<label> --tail=100
```

The usual causes are an image pull failure, a volume that will not bind, a
namespace quota with nothing left in it, a missing `google-oauth-client` Secret
(the daemon exits at boot without an OAuth client), and, with user namespaces on,
a node that does not support `hostUsers`. Re-running
`PUT /v1/tenants/<label>/credentials` is safe: every apply is a server-side
apply, so a retry converges rather than duplicating.

**Re-consent, and what "converges" means for a credential.** A tenant's daemon
reads a COPY of the sealed blob on its own volume, because a Secret mount is
read-only and the file is rewritten on every token refresh. So storing a new
blob is not by itself enough to change what a daemon uses. Two things make it
land: the ciphertext's hash rides in an annotation on the pod template, so a new
blob is a new pod spec and the Deployment rolls; and the init container keeps a
marker of what it last installed and re-copies exactly when the mounted Secret
differs from it. A daemon-refreshed credential survives every restart; a
genuinely new one installs.

The rule that falls out of it, worth knowing before you need it:

| Tenant is | `PUT /credentials` |
|---|---|
| `pending` | provisions it |
| `failed` | converges: re-applies and rolls the pod |
| `stopped` | brings it back with the new credential |
| `active` | `409`. Re-consent for a running tenant is `DELETE` (which keeps the mail and both Secrets) and then `PUT`. |

**Pairing a second device.** `POST /v1/tenants/<label>/pair` re-mints a code by
exec'ing into the tenant's pod. This supersedes the previous one, which is the
daemon's documented behaviour: one live pairing code per account.

**Upgrading the daemon.** Change `SQUELCH_WARDEN_IMAGE` in
`15-warden-config.yaml`, apply it, and restart the warden — one value, in the one
object both renderers read. Existing tenants do NOT move on the warden's
rollout, by design: it writes a tenant's objects at provision time and never
revisits them. The roller is what moves them: ONE tenant per tick, that rollout
finished before the run exits, and the next tick re-reads the whole fleet before
picking the next one — never all at once, because an upgrade that touches every
mailbox in one pass is an outage waiting for a bad release. A fleet with ten
tenants behind takes ten ticks, and the run says how many are left. It moves them
onto a new
DEPLOYMENT only: a change that lands on a tenant's Service, Ingress,
NetworkPolicy or PVC is invisible to it and wants `squelch-control reconcile`
per tenant. Exit codes and the levers: `PRODUCTION.md`, "Rolling the daemon
image".

**Backups.** Three things, of very different sizes:

- The Secrets in `tenants`. A few kilobytes, and every tenant's ability to read
  their own mail depends on theirs. `kubectl -n tenants get secret -o yaml`
  produces base64 of the DECRYPTED values, so treat that output the way you
  would treat the keys themselves.
- `/etc/litestream/backup-age.key`. Under a kilobyte, and every tenant's backup
  in R2 depends on it. Same custody as the Secrets dump; see §11 step 3.
- The PersistentVolumes (under `/var/lib/rancher/k3s/storage/`, or wherever
  local-path was repointed). The mail indexes. Bigger, and rebuildable from Gmail
  given a working credential.

Back up the first even if you skip the second. Without a tenant's identity, that
tenant's stored credential is unrecoverable and they consent with Google again.

### Backups today, stated honestly

What is actually running on the install this runbook was written from, as of
2026-08-11. Three mechanisms, each covering a different half of a different
problem, and one gap that is still open:

- **Provider server backups are ROOT DISK ONLY.** Hetzner's backups (7 daily
  snapshots) image the server's own disk. They do **not** include attached block
  volumes, ever, and nothing in the console says so at the moment you enable
  them.
- That happens to cover the load-bearing half. The k3s datastore lives on root
  (`/var/lib/rancher/k3s/server/db`), and it holds every tenant's identity
  Secret. Encrypted at rest, so the snapshot is not a pile of keys — but it is
  the difference between "restore" and "every tenant re-consents".
- It does **not** cover the mailboxes, because they are on the block volume.
- **Litestream IS wired up** (built 2026-08-10, retargeted to the 0.3.13 line
  2026-08-11, §11 above). One host-level systemd service streams every tenant's
  SQLite to Cloudflare R2 continuously, at `tenants/<label>/store.db`,
  discovered automatically within two minutes of a tenant existing. That closes
  the mailbox half: losing the volume is now minutes of mail rather than every
  index on the box.
- **It is encrypted with a key we hold.** Every snapshot and WAL segment is
  age-sealed to `LITESTREAM_AGE_RECIPIENT` before upload, so R2 holds ciphertext
  and Cloudflare cannot read a tenant's mail index. The private half lives at
  `/etc/litestream/backup-age.key` and in the password manager, and **nowhere
  else** — losing it makes every backup permanently unreadable. That is the
  trade this bought, and it is why litestream is pinned to 0.3.13 (§11 step 1).
- **What Litestream does NOT do.** It backs up **only SQLite**. The identity
  Secrets are in the k3s datastore, not on the volume, and the sealed
  `credentials.json` is on the volume but is not a database — so neither is in
  R2. The remaining gap is not disclosure any more; it is that the Secrets are
  still not on any schedule.

So, in plain terms, what each loss costs:

| What you lose | What it costs |
|---|---|
| the block volume | minutes of mail. `litestream restore` per tenant from R2 (§11.7); no re-sync from Gmail, no tenant action |
| the block volume, and R2 with it | every tenant re-syncs from Gmail — slow, noisy, but nothing is gone |
| one tenant's database (corruption, bad restore) | `litestream restore` to scratch, verify, swap it in with the pod scaled to 0 (§11.6). Point-in-time works too |
| the root disk, without a snapshot | every tenant's identity Secret, therefore every sealed credential, therefore a re-consent each. There is no escrow; we cannot recover this for them. **Litestream does not help here** |
| one tenant's identity Secret | that one tenant re-consents |
| the R2 bucket, to an attacker | nothing readable. It is age ciphertext, and the key is not on Cloudflare's side of the wire. Rotate the bucket token anyway |
| the R2 bucket **and** the backup age key, to an attacker | disclosure of mail indexes. Still not a path to any mailbox, credential or Google account: no identities and no credentials are in there |
| the backup age key, to nobody (just lost) | every backup in R2 is permanently unreadable, fleet-wide, and nothing tells you until you try to restore. Password manager. Today |

The shape to take away: **Litestream covers the big, noisy half; the Secrets are
still the whole recovery story and they are still not covered by anything on a
schedule.** So keep server backups ON, and if you take nothing else off this box
regularly, take `kubectl -n tenants get secret -o yaml` and
`/etc/litestream/backup-age.key` — encrypted, somewhere that is not this box.
They are kilobytes, and they are the things neither Hetzner nor R2 will hand
back to you.

## What this does and does not isolate

The hosted pitch is per-tenant isolation, so here is exactly where that claim
starts and stops.

**What holds:**

- **Separate pods, separate volumes, separate SQLite files.** Nothing is shared
  between tenants at the application layer.
- **A separate age identity per tenant.** A leaked Secret is one tenant. The
  control plane holds recipients only and can never open anything. There is no
  escrow: we cannot read a tenant's credential either, which is also why we
  cannot recover one for them.
- **Secrets encrypted at rest**, via `--secrets-encryption`. A stolen datastore
  file is not a stolen key.
- **No tenant-to-tenant network reachability.** Each tenant's NetworkPolicy
  accepts connections only from the ingress controller, on one port, and its
  egress is DNS plus TCP 443 to the public internet with every RFC 1918 range,
  the CGNAT range, and link-local subtracted. On a default k3s that removes the
  pod CIDR, the service CIDR, the in-cluster API server, the warden, and every
  cloud metadata endpoint in one stroke. There is also a namespace-wide
  default-deny underneath.
- **No API access from a tenant pod.** `automountServiceAccountToken: false`,
  so there is no token to use even if the network allowed it.
- **A pod that has given up everything it can.** Non-root at a fixed high uid,
  no privilege escalation, all capabilities dropped, `RuntimeDefault` seccomp, a
  read-only root filesystem with exactly two writable mounts, and (by default)
  its own user namespace so uid 0 inside is nobody outside. Pod Security
  Admission enforces `restricted` on the namespace, so that shape is the only
  shape anything here may take, whoever creates it.
- **Bounds on every container.** CPU, memory and ephemeral storage are requested
  and limited per container, `/tmp` has a size limit, and a LimitRange plus a
  ResourceQuota bound the namespace in aggregate. One tenant cannot starve the
  others or fill the node's disk out from under them.
- **A provisioner that cannot destroy mail.** The warden's Role has no `delete`
  on PersistentVolumeClaims, and no code path anywhere deletes a credential
  Secret. It does hold `delete` on Secrets, for one job: collecting the identity
  Secret of a tenant that is still `pending` past the TTL. That is the honest
  shape of it. The guarantee for a tenant that ever got as far as a credential
  is enforced by the code and its tests rather than by RBAC.

**What does not hold, and you should say so out loud if anyone asks:**

- **The kernel is shared.** User namespaces, seccomp and dropped capabilities
  raise the cost of a container escape; they do not make one impossible. A
  kernel bug is every tenant on the box. The named next rung is microVM
  isolation (Kata, Firecracker), where each tenant gets its own kernel. It is a
  runtime change, not an application change, and this design does not preclude
  it.
- **Root on the node reads everything.** Encryption at rest protects a stolen
  disk and a leaked backup. It does not protect against someone with a shell on
  the running box, who can read the decryption key and every mounted volume.
- **The warden's token is the tenant namespace.** Anyone holding it can create
  workloads there and exec into tenant pods. It cannot delete a volume, it
  cannot reach outside that namespace, and Pod Security Admission decides what
  kind of workload it may create, but that is the boundary, and there is no
  second factor between the control plane and it.
- **We hold the Google web client, and every tenant pod holds it too.** It has
  to be there: a refresh token only works for the client that minted it. On its
  own it opens no mailbox, but it is one more thing on the box that a hosted
  tenant is trusting us with, and the self-host tier exists precisely for people
  who would rather not.
- **A tenant can reach the ingress controller's public address**, the same as
  any stranger on the internet, because it is a public IP on port 443. That is
  not a path into another tenant's pod; it is the same front door everyone else
  uses.
- **One node is one failure domain.** No high availability, and a node loss is
  a restore from backup.

## The agent door is not served

Hosted publishes the human door only. Each tenant's Ingress declares exactly
three path prefixes, `/client`, `/console` and `/t`, and nothing else. `/mcp`
matches no rule, so Traefik answers its own 404 for it, as it does for every
other path on a tenant vhost.

That is an allowlist, not a deny rule, and deliberately so: a deny rule has to
enumerate every spelling of the thing it is refusing and fails open when it
misses one. This fails closed. The cost is that a new unauthenticated route in
the daemon needs a line in `HUMAN_DOOR_PREFIXES` in
`squelch-warden/src/objects.rs`, which is a list in code with a test on it
rather than a controller annotation nobody reads.

The daemon still serves `/mcp` inside its own pod. Nothing off the box can
reach it, and neither can another tenant.
