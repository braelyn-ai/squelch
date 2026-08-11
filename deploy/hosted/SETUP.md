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
complaint, set `SQUELCH_WARDEN_USER_NAMESPACES=off` in `20-warden.yaml`, restart
the warden, and read "What this does and does not isolate" below to understand
what you gave up.

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
this volume is now deliberately not the root disk. Read "Backups today, stated
honestly" under Operating notes before you assume otherwise.

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

Two images: the daemon every tenant runs, and the warden.

```sh
git clone https://github.com/braelyn-ai/squelch && cd squelch

docker build -f squelchd/Dockerfile -t ghcr.io/braelyn-ai/squelchd:v0.1.0 .
docker build -f Dockerfile.warden -t ghcr.io/braelyn-ai/squelch-warden:v0.1.0 .
docker push ghcr.io/braelyn-ai/squelchd:v0.1.0
docker push ghcr.io/braelyn-ai/squelch-warden:v0.1.0
```

Tag them. The warden refuses to start with an untagged tenant image, because an
untagged image means every tenant's daemon silently changes version on any pod
restart, which is an upgrade nobody scheduled and nobody can roll back.

If the packages are private, create the pull secret and uncomment
`SQUELCH_WARDEN_IMAGE_PULL_SECRET` in `20-warden.yaml`:

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

## 7. Tenant limits

```sh
kubectl apply -f deploy/hosted/70-tenant-limits.yaml
```

A LimitRange and a ResourceQuota on `tenants`. **Size them to your box before
you apply them**; the shipped numbers assume the 2 vCPU / 4 GB floor above and
are deliberately conservative.

The warden already puts requests and limits on both containers of every tenant
pod (`SQUELCH_WARDEN_CPU_REQUEST` and friends in `20-warden.yaml`, defaulting to
100m/256Mi requested and 1000m/1Gi allowed, plus a 512Mi cap on the pod's `/tmp`
and an ephemeral-storage bound so a runaway tenant cannot fill the node's root
filesystem). This file is the layer under that: defaults for anything that lands
in the namespace without bounds of its own, and an aggregate ceiling.

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

Edit `20-warden.yaml`: the four places marked `EDIT ME` (the warden image, your
base domain, the squelchd image tenants run, and the warden's hostname in two
spots on the Ingress). Then:

```sh
kubectl apply -f deploy/hosted/10-warden-rbac.yaml
kubectl apply -f deploy/hosted/30-tenants-default-deny.yaml
kubectl apply -f deploy/hosted/20-warden.yaml

# Check the API server's in-cluster address matches the policy before applying it.
kubectl -n default get svc kubernetes -o jsonpath='{.spec.clusterIP}'; echo
kubectl apply -f deploy/hosted/50-warden-networkpolicy.yaml

kubectl -n warden rollout status deploy/squelch-warden
```

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
# put that address as a /32 in SQUELCH_WARDEN_NODE_CIDR in 20-warden.yaml
kubectl apply -f deploy/hosted/20-warden.yaml
kubectl -n warden rollout restart deploy/squelch-warden
```

Then delete and re-provision the canary. The warden adds one ingress rule to
every tenant's policy: that CIDR, TCP 8848, nothing else.

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

Each tenant's daemon downloads about 130 MB of ONNX weights the first time it
embeds a message, into `$HOME/.local/share/squelch/models`, and `HOME` is that
tenant's own volume. Left alone, that is the same download once per tenant,
inside a signup somebody is watching.

**The chosen mechanism: one shared read-only volume, copied into each tenant's
volume by its init container.** The warden mounts the shared PVC into the init
container only, which copies the cache across if the tenant does not have one
yet. It is a copy rather than a symlink because the daemon's root filesystem is
read-only and fastembed expects to own its cache directory; the cost is ~130 MB
of local disk per tenant, which is nothing next to a mail index.

Fill it once, from a tenant that has already downloaded them:

```sh
kubectl apply -f deploy/hosted/60-models.yaml
kubectl -n tenants wait --for=condition=Ready pod/squelch-models-seed

# From a tenant that has synced at least once:
POD=$(kubectl -n tenants get pod -l app.kubernetes.io/instance=<first-label> -o name | head -1)
kubectl -n tenants cp "${POD#pod/}:/data/.local/share/squelch/models" ./models
kubectl -n tenants cp ./models squelch-models-seed:/seed

# kubectl cp lands it one level deep; flatten it so the PVC root IS the cache.
kubectl -n tenants exec squelch-models-seed -- sh -c 'mv /seed/models/* /seed/ && rmdir /seed/models'
kubectl -n tenants exec squelch-models-seed -- ls /seed

kubectl -n tenants delete pod squelch-models-seed
```

Then uncomment the `SQUELCH_WARDEN_MODEL_PVC` env entry in `20-warden.yaml`,
re-apply it, and `kubectl -n warden rollout restart deploy/squelch-warden`. Every
tenant provisioned after that skips the download. Tenants that already have their
own copy are unaffected.

The `ReadWriteOnce` volume is mounted by many pods, which is legal because they
are all on the one node. **If you ever add a second node, this stops working.**
The answer then is the other mechanism: build a squelchd image with the weights
baked in at `/data/.local/share/squelch/models` and point
`SQUELCH_WARDEN_IMAGE` at it. That costs image size and gains node
independence.

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

**Upgrading the daemon.** Change `SQUELCH_WARDEN_IMAGE` and restart the warden,
then re-apply each tenant. Existing tenants do NOT move on their own, by design.
There is no "upgrade all tenants" button and that is deliberate: an upgrade that
touches every mailbox at once is an outage waiting for a bad release.

**Backups.** Two things, of very different sizes:

- The Secrets in `tenants`. A few kilobytes, and every tenant's ability to read
  their own mail depends on theirs. `kubectl -n tenants get secret -o yaml`
  produces base64 of the DECRYPTED values, so treat that output the way you
  would treat the keys themselves.
- The PersistentVolumes (under `/var/lib/rancher/k3s/storage/`, or wherever
  local-path was repointed). The mail indexes. Bigger, and rebuildable from Gmail
  given a working credential.

Back up the first even if you skip the second. Without a tenant's identity, that
tenant's stored credential is unrecoverable and they consent with Google again.

### Backups today, stated honestly

What is actually running on the install this runbook was written from, as of
2026-08-10, is one mechanism, and it is not the one anybody would design:

- **Provider server backups are ROOT DISK ONLY.** Hetzner's backups (7 daily
  snapshots) image the server's own disk. They do **not** include attached block
  volumes, ever, and nothing in the console says so at the moment you enable
  them.
- That happens to cover the load-bearing half. The k3s datastore lives on root
  (`/var/lib/rancher/k3s/server/db`), and it holds every tenant's identity
  Secret. Encrypted at rest, so the snapshot is not a pile of keys — but it is
  the difference between "restore" and "every tenant re-consents".
- It does **not** cover the mailboxes, if the block volume above is in use. That
  is the deliberate trade, not an oversight: mail is the big, rebuildable half.
- **Litestream is not wired up.** `docs/HOSTED.md` once promised streaming SQLite
  backup to object storage "from day one"; the k3s design dropped it and nothing
  replaced it. It is still owed.

So, in plain terms, what each loss costs:

| What you lose | What it costs |
|---|---|
| the block volume | every tenant re-syncs from Gmail — slow, noisy, but nothing is gone |
| the root disk, without a snapshot | every tenant's identity Secret, therefore every sealed credential, therefore a re-consent each. There is no escrow; we cannot recover this for them |
| one tenant's identity Secret | that one tenant re-consents |

Which is why, until Litestream lands: keep server backups ON, and if you take
nothing else off this box on a schedule, take
`kubectl -n tenants get secret -o yaml` — encrypted, somewhere that is not this
box. It is kilobytes and it is the whole recovery story.

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

Hosted publishes the human door only. Each tenant's Ingress declares exactly two
path prefixes, `/client` and `/t`, and nothing else. `/mcp` matches no rule, so
Traefik answers its own 404 for it, as it does for every other path on a tenant
vhost.

That is an allowlist, not a deny rule, and deliberately so: a deny rule has to
enumerate every spelling of the thing it is refusing and fails open when it
misses one. This fails closed. The cost is that a new unauthenticated route in
the daemon needs a line in `HUMAN_DOOR_PREFIXES` in
`squelch-warden/src/objects.rs`, which is a list in code with a test on it
rather than a controller annotation nobody reads.

The daemon still serves `/mcp` inside its own pod. Nothing off the box can
reach it, and neither can another tenant.
