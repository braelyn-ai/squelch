# Rolling a new image onto every pod

The procedure, start to finish, for getting a freshly cut `daemon-X.Y.Z` onto
everything that runs it. This page is the checklist; it links out rather than
re-explaining. The reasoning lives in `PRODUCTION.md` ("Rolling the daemon
image"), `../../docs/RELEASING.md` (surfaces and tags), and
`../../squelch-warden/README.md` (the roller).

Budget: ten minutes of typing, then **one roller tick per tenant that is
behind** — fifteen minutes each, unattended. A four-tenant fleet is done in an
hour. Nothing below needs watching while it converges.

---

## What "every pod" means: four pins, three surfaces

One tag, written in four places, because Kubernetes has no single object that
can hold all of them.

| What runs it | Image | Pinned in | Moves when |
|---|---|---|---|
| Tenant daemons, one pod per mailbox | `squelchd` | `15-warden-config.yaml` → `SQUELCH_WARDEN_IMAGE` | the roller converges them, **one per tick** |
| The warden | `squelch-warden` | `20-warden.yaml` → `image:` | you apply it |
| The roller | `squelch-warden` | `90-warden-roller.yaml` → `image:` (**must equal the line above**) | its next tick |
| The model-seed pod | `squelchd` | `60-models.yaml` → `image:` | only when you re-seed weights; not part of a routine rollout |
| `control` (signup.passband.app) | `squelch-control` | not pinned | **merge to `main`** — Railway deploys it, independent of all of the above |

That last row is the one that catches people. `control` ships on merge and the
carrier ships on `kubectl apply`, so a change that spans both is out of sync for
however long you leave between them. If they must move together, do the carrier
promptly after the merge.

---

## 0. Preflight

Do not skip this. Rolling onto a fleet that is already unhappy conflates two
problems, and the roller will stop on the old one before it reaches your new
image.

```sh
ssh carrier
kubectl -n warden create job --from=cronjob/squelch-warden-roll preflight
kubectl -n warden logs -f job/preflight
kubectl -n warden delete job preflight
```

Read the summary. You want `0 checked, ... already current` with no other lines.
Anything else, deal with it now:

- **`needs a person (another field manager owns part of the Deployment)`** —
  those tenants will **not** move in this rollout, today or ever, until somebody
  runs `squelch-control reconcile <label>`. Decide now whether you are shipping
  a split-version fleet.
- **`needs a person (identity Secrets whose label does not validate)`** — same,
  permanently. See PRODUCTION.md for how to find them.
- **`needs a person (a workload whose sealed credential Secret is gone…)`** —
  also permanent, and it means that mailbox's owner has to re-consent before
  anything can render them.
- **`DOWN`** or **`HALTED`** — fix before you bump anything.

Suspend the timer while you work, so a scheduled tick does not start mid-edit:

```sh
kubectl -n warden patch cronjob squelch-warden-roll -p '{"spec":{"suspend":true}}'
```

---

## 1. Cut the tag

The workspace version and the tag must agree or CI fails before it writes to the
registry.

```sh
# on main, with the version bumped in Cargo.toml and committed
git tag daemon-X.Y.Z
git push origin daemon-X.Y.Z
gh run watch
```

## 2. Verify all three images published

The failure mode is a **half-published release**: one image lands, another
fails, and you pin a tag that only two of the three have.

```sh
for img in squelchd squelch-warden squelch-control; do
  TOKEN=$(curl -s "https://ghcr.io/token?scope=repository:braelyn-ai/$img:pull" \
    | sed -n 's/.*"token":"\([^"]*\)".*/\1/p')
  echo "== $img"
  curl -s -H "Authorization: Bearer $TOKEN" "https://ghcr.io/v2/braelyn-ai/$img/tags/list"
done
```

All three must list `daemon-X.Y.Z`. `squelchd` is amd64 + arm64; warden and
control are amd64 only, which is fine because the carrier is amd64.

## 3. Repoint the pins

Four lines, in the repo, committed:

```
deploy/hosted/15-warden-config.yaml   SQUELCH_WARDEN_IMAGE: ghcr.io/braelyn-ai/squelchd:daemon-X.Y.Z
deploy/hosted/20-warden.yaml          image: ghcr.io/braelyn-ai/squelch-warden:daemon-X.Y.Z
deploy/hosted/90-warden-roller.yaml   image: ghcr.io/braelyn-ai/squelch-warden:daemon-X.Y.Z
deploy/hosted/60-models.yaml          image: ghcr.io/braelyn-ai/squelchd:daemon-X.Y.Z   (only if re-seeding)
```

**Never set these as a container `env:` in a pod spec.** A container `env:` name
outranks the same name from `envFrom`, silently, and that is how the warden and
the roller end up rendering two different tenants forever, with `drift` reading
clean the whole time. One ConfigMap, both pods.

## 4. Apply, in numbered order

```sh
ssh carrier
# FIRST when this release adds a verb, LAST when it drops one — see below.
# Skip entirely when the file has not changed.
kubectl apply -f deploy/hosted/10-warden-rbac.yaml
kubectl apply -f deploy/hosted/15-warden-config.yaml
kubectl apply -f deploy/hosted/20-warden.yaml
kubectl apply -f deploy/hosted/90-warden-roller.yaml
```

**RBAC ordering depends on which way the verbs move, and getting it backwards
is a 403 in production.** Diff `10-warden-rbac.yaml` against what is live and
ask which of the two this release is:

- **The Role GAINS a verb** (the new image calls something the old one did
  not): apply the Role **first, before the image**. A warden that reaches for a
  verb its Role does not carry gets a 403 and halts.
- **The Role DROPS a verb** (the new image stopped calling it): apply the Role
  **last, after the image**. A Role trimmed while the old pod is still serving
  takes the verb out from under a caller that still uses it.

The upgrade this page was written for is the first kind: `get` on services
returns to the Role as a migration bridge for tenants cancelled before the
cancellation marker existed, so **the Role goes on before the warden image**.
That bridge, its RBAC verb and `Cluster::get_service` are deleted together once
no unmarked cancelled tenant is left; that removal will be the second kind.

## 5. Restart the serving warden, in the same minute

```sh
kubectl -n warden rollout restart deploy/squelch-warden
kubectl -n warden rollout status deploy/squelch-warden
curl -sS https://warden.passband.app/healthz    # -> ok
```

`envFrom` is read once, at pod start. The roller is a fresh pod every tick and
picks the new pin up on its own; **the serving warden does not**. Until it
restarts it goes on rendering the old image into every new signup and every
`llm mint`, and the roller then rolls those tenants forward again — churn, not
damage, but avoidable. Apply and restart together.

Then let the timer run again:

```sh
kubectl -n warden patch cronjob squelch-warden-roll -p '{"spec":{"suspend":false}}'
```

## 6. Let it converge: one tenant per tick

The roller converges **one** tenant per run and exits. This is the safety model,
not a throughput bug: a finished rollout only proves the API server saw a ready
replica, and squelchd opens its socket before it finishes starting, so what
actually clears a render is a quarter hour of a real daemon serving real mail
plus the next tick's refusal to roll anything if that mailbox is not up.

```sh
kubectl -n warden get jobs -w                 # one row per run
kubectl -n warden logs job/<name> | tail -20  # the summary
```

**Expect failed Jobs. That is the design.** Exit 3 means "rolled one, more
queued", and Kubernetes only knows zero and non-zero, so a five-tenant bump
leaves four failed Jobs and then a green one. The summary line to watch:

```
  still behind, one per run: 3 more, the next at the next tick
```

That number falling by one each tick is a healthy rollout. That number **not
falling** is a stall — see below.

To go faster than the schedule, the safe manual-run recipe (suspend, check
nothing is active, run, delete the Job, unsuspend) is in
`90-warden-roller.yaml`'s header. Do not skip the suspend: a hand-made Job is
not counted by `concurrencyPolicy: Forbid`, so two rollers can walk the fleet at
once with two mailboxes down.

## 7. Verify

```sh
# every tenant's daemon image, one line each
kubectl -n tenants get deploy -l app.kubernetes.io/managed-by=squelch-warden \
  -o custom-columns='TENANT:.metadata.name,IMAGE:.spec.template.spec.containers[0].image'

# the two warden pins, which must be identical
kubectl -n warden get deploy/squelch-warden \
  -o jsonpath='{.spec.template.spec.containers[0].image}{"\n"}'
kubectl -n warden get cronjob/squelch-warden-roll \
  -o jsonpath='{.spec.jobTemplate.spec.template.spec.containers[0].image}{"\n"}'
```

Done when a run exits **0** with everything under `already current`. A dry run
is the cheapest way to ask:

```sh
kubectl -n warden create job --from=cronjob/squelch-warden-roll verify
kubectl -n warden logs -f job/verify && kubectl -n warden delete job verify
```

(That Job runs `roll` for real, not `--dry-run` — it takes a tenant if one is
behind. If you want reads only, edit `args:` to `["roll", "--dry-run"]` in a
copy of the manifest.)

---

## When it goes wrong

| Symptom | Exit | What it means | Do this |
|---|---|---|---|
| `still behind: N more`, N not falling across runs | 3 | **Stall.** The same tenant is first in the queue every tick and its apply is refused, so nothing behind it moves. | `kubectl -n warden logs job/<name>` for the machine reason (`volume_failed`, `service_failed`, `workload_failed`…). Usually a config value the API server will not accept — often not the one you meant to change. Suspend, revert it, restart the warden, unsuspend. |
| `HALTED before applying anything` | **4** | **Casualty — the fleet is FROZEN.** A tenant carries the new render and is not serving it, so the render is the suspect. Nothing was applied to anyone, and nothing will converge on any tick until this is dealt with. | Suspend immediately. Look at that pod. This is the roller telling you the release is bad, and it is the one exit code that should page. |
| `HALTED on <label>` | 1 | That tenant's rollout did not finish. It is the only one written to, and it goes back on the queue. | `kubectl -n tenants logs deploy/<label>` and `describe pod -l app.kubernetes.io/instance=<label>`. |
| `DOWN (no workload, and nothing recorded a cancellation)` | 1 | A job nobody finished left a mailbox with no pod. Most often a `reconcile` the CLI hung up on between the delete and the apply. The roller will not repair one unattended. | `squelch-control reconcile <label>`, and read "A reconcile can outrun the CLI" below first. |
| `needs a person (a workload whose sealed credential Secret is gone…)` | 1 | That tenant cannot be rendered at all, so no run will ever converge it. The walk continues past it rather than halting, but the fleet is not converged while it is there. | The credential is unrecoverable from here: the owner re-consents, and `PUT /v1/tenants/<label>/credentials` puts them back. |
| `needs a person (another field manager…)` | 2 | Somebody's `kubectl` owns part of that Deployment. It is out of every roll until repaired. | `squelch-control drift <label>`, then `reconcile <label>` when you can afford that mailbox to blip. |
| Nothing at all in the log, one sentence | 1 | It never started: a refused config value, or an API server it could not reach. | Check `15-warden-config.yaml` against `squelch-warden/README.md`'s env table. |

### A reconcile can outrun the CLI

`squelch-control reconcile <label>` waits for the warden to finish, and on the
delete-recreate path finishing takes minutes: delete the Deployment, wait for the
old pod to let go of the `ReadWriteOnce` volume, apply five objects, wait for a
ready replica. Both waits are bounded by `SQUELCH_WARDEN_READY_TIMEOUT_SECS`,
180 seconds each by default, so the honest ceiling is around six minutes.

**Hanging up on it does not just lose the answer, it stops the work.** The
warden's reconcile lives in the request handler: when the client gives up,
reqwest drops the connection, axum drops the handler future, and the warden stops
wherever it had got to. If that is between the delete and the apply, the mailbox
is down and nothing is coming to recreate it. That is not a hypothetical: on
2026-08-19 it took one tenant down for eight minutes, and the roller's next tick
reporting it as `DOWN` is how anyone found out.

So: **the CLI's answer is not the record of what happened. The warden's log is.**
Two different failures, two different messages, and they mean opposite things:

- `did not answer in time and may still be working` — the call landed. The
  warden may be mid-operation right now. **Do not retry blind.** Read the log
  first, then look at the object:

  ```sh
  kubectl -n warden logs deploy/squelch-warden | grep -E 'tenant=<label>($| )'
  kubectl -n tenants get deploy <label>     # NotFound means it stopped mid-recreate
  ```

  The `($| )` is not decoration and a trailing space will not do instead. See
  PRODUCTION.md, "Shipping a tenant-shape change", for the two ways the obvious
  greps are wrong.

  Once the old pod is gone, running the same `reconcile` again resumes rather
  than refusing (see PRODUCTION.md, "A reconcile that died in its own
  delete/apply window").
- `could not be reached` — connection refused, DNS, TLS. The call never landed
  and nothing was started. Fix the reachability and run it again.

The CLI now allows **ten minutes** for `reconcile` alone (`RECONCILE_TIMEOUT` in
`squelch-control/src/config.rs`; every other call, `drift` included, keeps 30
seconds). That covers the default warden with room to spare. **If you ever raise
`SQUELCH_WARDEN_READY_TIMEOUT_SECS` above 300, raise that constant with it** —
`control` cannot see the warden's configuration, so nothing enforces the pair and
the symptom is this incident again.

To run one past the CLI's budget entirely, call the warden yourself with a
timeout you choose:

```sh
TOKEN=$(kubectl -n warden get secret squelch-warden -o jsonpath='{.data.token}' | base64 -d)
WARDEN_URL=https://warden.passband.app
curl -sS -X POST -m 600 -H "Authorization: Bearer $TOKEN" \
  "$WARDEN_URL/v1/tenants/<label>/reconcile"
```

`-m` is a hang-up like any other, so it buys a longer window and not a safer one.
Pick a number you are willing to wait out, and if it does expire, go read the log
rather than pressing up-enter.

### Rolling back

There is no rollback command, and there does not need to be one: a rollback is a
rollout to the previous tag. Repoint the four pins at `daemon-(X.Y.Z-1)`, apply,
restart the warden, and let the roller walk the fleet back — one tenant per
tick, same as forward. Tenants already on the bad image come back first only if
they happen to sort first; there is no priority ordering.

If the bad image will not start at all, the roller stops on the first casualty
and refuses to touch anyone else, so the blast radius is one mailbox and the
rollback has fewer tenants to undo than you would fear.

---

## The five things that actually bite

1. **A hand-edited tenant never moves.** `kubectl set env`, `kubectl edit`,
   `kubectl scale`, and above all `kubectl rollout restart` — the first thing
   anyone does to a sick mailbox — record that person as an owner of those
   fields, and the roller refuses to touch a Deployment it does not solely own.
   Undoing the edit does not help: the ownership tombstone survives. Only
   `squelch-control reconcile <label>` clears it, and that is the command that
   deletes and recreates the Deployment — read "A reconcile can outrun the CLI"
   above before you run it, because interrupting it leaves the mailbox down.
2. **One sick mailbox freezes the whole fleet.** The casualty rule stops the run
   before it writes anything — not just that tenant, everything.
3. **`imagePullPolicy: IfNotPresent` plus a moved tag means no pull.** Roll
   forward with a new tag; never repoint an existing one.
4. **The roller converges the Deployment and nothing else.** A release that
   changes a tenant's Service, Ingress, NetworkPolicy or PVC reaches new signups
   only. No roll will ever deliver it, and every drift report will call those
   tenants current. Those want `squelch-control reconcile` per tenant.
5. **`control` already shipped.** It deployed the moment you merged, before any
   of this. If the release pairs a control change with a carrier change, that
   gap is real and nothing enforces the pairing.
