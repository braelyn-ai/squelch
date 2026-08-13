# Package tracking

Squelch pulls packages out of your mail into a `shipments` table and serves it on
both doors (`GET /client/shipments`, and `get_shipments` on the agent door). Two
mechanisms feed that table, and they are independent:

- **Identity**: which package an email is about. Always on.
- **Freshness**: where that package is right now. Off until you bring carrier API
  credentials.

This document is the operator's view of both. Neither one ever sees sealed mail:
seal detection runs before shipment extraction and before anything else reads a
body, and sealing a message afterwards deletes the shipment rows it fed. See
[SECURITY.md §4](SECURITY.md).

## Why either exists

**Mail is a stale snapshot.** A package's status is only ever as current as the
last email about it, and the useful email is the one that most often never
arrives. Plenty of retailers send a ship notice and then nothing, so the row sits
at `shipped` forever and "delivered" is something you learn by looking at the
porch. Polling the carrier makes the carrier the ground truth between emails.

**A regex cannot tell a tracking number from an item id.** They are the same
shape. A FedEx number can be 12 digits, and so can an eBay item number, and one
eBay order thread carries half a dozen of those. An Amazon marketing mail
("review your upcoming delivery") carries a 10-digit run right next to a DHL
mention, which is the DHL shape plus the DHL signal. The old detector minted
phantom packages out of both. The detector is stricter now, and an LLM extractor handles the mail a
regex was never going to get right at all: the order confirmation that carries no
tracking number, only an order reference.

Neither mechanism needs the other. Detection with no carrier credentials is the
old email-only behaviour. Carrier polling with no LLM key still polls every
number the regex is confident about.

## Carrier polling

### Credentials are the feature flag

There is no `enabled = true` anywhere in this feature. A carrier is polled if and
only if its credentials resolve, and when none of the four resolve, the daemon
spawns no poller task and contacts no carrier API at all. Half a credential pair is off, and
a blank string is treated as absent, so a leftover `client_id = ""` cannot
half-enable UPS and fire an empty string at somebody's token endpoint.

The daemon says which state it is in at startup:

```
squelchd: shipment poller enabled (carriers: dhl, ups)
squelchd: shipment poller disabled (no carrier credentials in [carriers])
```

The second line is the normal one, not a warning.

### Getting the credentials

All four are bring-your-own. Squelch never proxies a carrier call, and there is
no shared key: the quota you spend is the one you signed up for.

| Carrier | Portal | You end up with | The wall |
|---|---|---|---|
| UPS | [developer.ups.com](https://developer.ups.com) | Client ID + Client Secret | A UPS shipping account number, before you get any credential |
| FedEx | [developer.fedex.com](https://developer.fedex.com) | API Key + Secret Key | A FedEx account number, but only to leave sandbox |
| DHL | [developer.dhl.com](https://developer.dhl.com) | One API key | Nothing to sign up. A review queue and a tight free tier |
| USPS | [developers.usps.com](https://developers.usps.com) | Consumer Key + Consumer Secret | A free business account, and 60 calls an hour |

The friction is not where most people expect it, so it is worth being specific.

**UPS is the hard one.** Create a developer account, register an application, and
add the **Tracking** product to it. The catch is that registering the application
asks you to attach a **UPS shipper or billing account number**, and there is no
sandbox tier that skips it. If you are not already a UPS shipping customer, you
cannot get a credential at all, not even to experiment. The result, once you are
through, is an ordinary OAuth client-credentials pair; squelch calls
`onlinetools.ups.com` and refreshes the token itself. UPS publishes no rate
limits for this API, which is why squelch imposes no budget on it beyond a
one-second floor between calls.

**FedEx** sequences the same requirement better. Sign up, create a project, and
add the **Track API** (it sits under the "Basic Integrated Visibility" tier;
"Advanced" is for authenticated-delivery shipments and is not what you want).
Test credentials are issued immediately with no account number. A **FedEx account
number** is required to promote the project to production, which is an explicit
step in the portal that reveals a separate Production Key tab. Take the
production pair: squelch only ever calls `apis.fedex.com` and has no sandbox
switch, so a test key authenticates against nothing here. The portal calls the
two halves API Key and Secret Key; they go in `client_id` and `client_secret`.

**DHL** is the only one with no carrier relationship to establish: register (it
asks for a company name and wants the email domain to match), verify the email,
subscribe your app to **Shipment Tracking - Unified**, and copy the key. There is
no OAuth exchange, the key is the whole credential, sent as a `DHL-API-Key`
header. Two things to plan around. The free tier is 250 calls per day at one call
per five seconds, which DHL itself describes as development-only, and anything
above it needs a written use-case request that has been reported to take several
business days. That tier is why DHL is the one carrier with a configurable daily
cap, defaulted to 200.

**USPS** needs a free USPS business account (the Business Customer Gateway, and
its Customer Onboarding Portal, where you create the app that mints the Consumer
Key and Consumer Secret). Expect business-account paperwork rather than a
one-click signup, but not a postage relationship: **CRID and MID, the mailer
identifiers that make USPS's label and manifesting APIs genuinely painful, are
not required for tracking-only access.** If an enrollment flow starts demanding a
Mailer ID for a read-only tracking integration, you are further down the shipping
path than you need to be. The real USPS constraint is the ceiling, not the
signup: the default is 60 calls per hour per consumer key, which squelch enforces
itself, and USPS grants increases on request.

**One USPS unknown worth testing before you rely on it.** USPS's API access page
says tracking access requires being "authorized to access the Mailer ID (MID)
included in the package barcode or the tracking number itself". Read strictly,
that describes tracking packages *you* mailed, which is the opposite of what this
feature does: every number here belongs to a package someone else sent you. In
practice the v3 tracking endpoint is widely used for inbound numbers, so this may
only govern the bulk webhook product. It is not resolvable from the
documentation, only by pointing a real consumer key at a real inbound number. If
USPS refuses, its client is severable: drop the `[carriers.usps]` block and the
other three carry on. Nothing else in squelch depends on it.

None of the four needs a write scope or the ability to create labels. Squelch
only ever reads a tracking number's status. Carriers are independent, so
configure whichever you can get today and add the rest later; the poller is happy
with one.

### Configuration

`[carriers]` in `~/.config/squelch/config.toml`. Every value can come from the
environment instead (see below), which is how a container is configured.

```toml
[carriers]
# Baseline cadence for an in-flight package.
poll_interval_hours = 6
# The tighter cadence for a package that is out for delivery.
ofd_poll_interval_mins = 60
# Stop polling a package this many days after it was first seen.
max_age_days = 45
# Consecutive permanent failures before a package is retired.
max_failures = 5

# Each block below is optional, and its presence is what enables that carrier.
[carriers.ups]
client_id = "..."
client_secret = "..."

[carriers.fedex]
client_id = "..."
client_secret = "..."

[carriers.usps]
consumer_key = "..."
consumer_secret = "..."

[carriers.dhl]
api_key = "..."
# Calls per day. Default 200, deliberately under DHL's 250/day free tier.
daily_cap = 200
```

The four knobs at the top only pace a poller that credentials have already turned
on. Setting them without any credentials changes nothing.

`poll_interval_hours = 0` or `ofd_poll_interval_mins = 0` is floored to 1 with a
warning on stderr, because a zero interval is a spin loop against somebody else's
rate-limited API. `max_failures = 0` is accepted and is a footgun: it makes
nothing pollable at all and hides every ambiguous row from the listing (see
[Retirement](#retirement-and-suppression)).

### The environment equivalents

| Variable | Sets |
|---|---|
| `SQUELCH_UPS_CLIENT_ID` / `SQUELCH_UPS_CLIENT_SECRET` | `[carriers.ups]` |
| `SQUELCH_FEDEX_CLIENT_ID` / `SQUELCH_FEDEX_CLIENT_SECRET` | `[carriers.fedex]` |
| `SQUELCH_USPS_CONSUMER_KEY` / `SQUELCH_USPS_CONSUMER_SECRET` | `[carriers.usps]` |
| `SQUELCH_DHL_API_KEY` | `[carriers.dhl] api_key` |
| `SQUELCH_DHL_DAILY_CAP` | `[carriers.dhl] daily_cap` |
| `SQUELCH_CARRIERS_POLL_INTERVAL_HOURS` | `poll_interval_hours` |
| `SQUELCH_CARRIERS_OFD_POLL_INTERVAL_MINS` | `ofd_poll_interval_mins` |
| `SQUELCH_CARRIERS_MAX_AGE_DAYS` | `max_age_days`, which is also the extractor's horizon (see [Identity](#identity-which-package-is-this)) |
| `SQUELCH_CARRIERS_MAX_FAILURES` | `max_failures` |

A credential pair set in the environment **materializes a carrier the config file
never mentions**, so a Docker deployment needs no `config.toml` at all.
`SQUELCH_DHL_DAILY_CAP` is the one exception to that rule: a cap alone never
conjures a carrier, so it is honored only when DHL already has a key.

Secrets are never logged. Each credential struct redacts its secret half, so a
stray debug print of the config shows `<redacted>` where the secret was.

### What the cadence actually does

The poller wakes every 5 minutes, works out which packages are due, and calls
carriers for those. `poll_interval_hours` is not a timer, it is the age at which
a row becomes eligible.

**Which packages are pollable at all.** Not delivered, carrier is one of the four
with an API, first seen within `max_age_days`, and fewer than `max_failures`
permanent failures. Never-polled rows go first, then least-recently-polled.

**Which of those are due.** A never-polled row is due immediately. Otherwise a row
is due `poll_interval_hours` after its last poll attempt, or
`ofd_poll_interval_mins` after it if the package is out for delivery (status
`out_for_delivery`, or a carrier ETA landing today). Each row's interval carries
a deterministic per-row jitter of plus or minus 10%, derived from its id, so a
batch of packages detected in one mail sweep spreads out instead of arriving at
the carrier together, and so the spread survives a restart.

**Pacing across and within carriers.** One task serves all carriers, picking the
one whose next allowed call comes soonest, so DHL's five-second floor never
blocks a UPS row. Within a carrier, calls are sequential and spaced by that
carrier's own minimum interval: 1 second for UPS, FedEx and USPS, 5 seconds for
DHL (its free tier allows one call per five seconds).

**Startup delay.** The poller waits a random 30 to 120 seconds after start before
its first pass, drawn once per daemon, so a fleet of restarted daemons does not
arrive at a carrier in lockstep.

**Budgets.** DHL is held to `daily_cap` calls per rolling 24 hours (default 200,
under the 250/day free tier). USPS is held to 60 calls per rolling hour, which is
not configurable: it is USPS's published per-key ceiling, not a budget you chose.
UPS and FedEx meter per second rather than per day, so they get no budget beyond
the one-second floor.

**Backoff.** A 429 puts that carrier to sleep for the `Retry-After` it sent
(clamped to at most an hour), with a floor of 15 minutes even if the carrier said
one second: a carrier answering 429 is describing its token bucket, and coming
straight back is how a soft limit becomes a blocked key. A 401 or 403 pauses that
carrier for an hour and prints one line:

```
squelch: ups rejected the configured credentials; pausing ups polls for 60 minutes
```

Once per cooldown, not once per pass. Nothing but fixing the credential resolves
it, and the retry exists so a corrected key is picked up without a restart.
Anything else (a transport error, a 500, an unparseable body) backs that carrier
off 5 seconds, doubling to a 5-minute ceiling, reset by the next success.

**All of this pacing state is in memory and resets when the daemon restarts:**
cooldowns, the backoff ladder, and the DHL and USPS budget windows. A daemon that
restarts every few minutes could spend more than a day's DHL cap in a day. That
is an accepted trade (the randomized startup delay covers the stampede case, and
the default cap sits well under the free tier), but it is worth knowing before
you put the daemon in a crash loop.

### Retirement and suppression

Only a carrier's "I have never heard of this number" counts against a package,
and only once the package is old enough to have been handed over: a rejection
inside the first 72 hours is recorded but not counted. Retailers routinely mail a
waybill before the parcel reaches the carrier, so a fresh 404 means "not yet",
not "never". Rate limits and auth failures are the carrier's problem rather than
the number's and never count. Transport errors do not count either, but they do
stamp the attempt, so one unanswerable number cannot hold the front of the queue
and starve every other package behind it.

At `max_failures` counted rejections the row leaves the pollable set. It also
disappears from `GET /client/shipments` and from the agent door's
`get_shipments`, but only if its tracking number is *ambiguous*, meaning it does
not identify its own carrier: a number in a shape a retailer item id shares, that
no carrier will acknowledge, was probably never a tracking number. A `1Z…`,
`TBA…` or IMpb row is never hidden however badly it polls.

Retirement is not permanent. The rows stay in the database, the listing filter is
read-side only, and either a successful poll or a new email that the state
machine accepts clears the counter and brings the package back.

### Forcing a pass

`POST /client/shipments/poll` kicks the poller immediately. It is human-door
only: the agent door reads the shipments table and never gets to spend your
carrier quota.

```sh
curl -sS -X POST -H "Authorization: Bearer $SQUELCH_API_TOKEN" \
  http://127.0.0.1:8848/client/shipments/poll
# => {"kicked":true,"carriers":["dhl","ups"]}
```

On a daemon with no carrier credentials:

```
{"kicked":false,"carriers":[]}
```

That is a 200 and a normal answer, not an error. A client can read the same two
fields either way and hide the button when the list is empty.

The kick starts a pass now; it does not bypass per-carrier cooldowns, budgets or
minimum intervals, so mashing it cannot spend a daily cap or earn a 429. It
returns as soon as the pass is scheduled, without waiting for the carrier round
trips, so re-read `GET /client/shipments` a few seconds later.

Nothing in Passband calls this endpoint yet; today it is a curl-and-scripts
affair.

### The new fields

`GET /client/shipments` rows gained five fields, all of which are filled by the
carrier and never by mail:

| Field | Meaning |
|---|---|
| `carrier_status_raw` | The carrier's own latest status string, verbatim. Recorded even when it maps to none of our five status rungs, so a client can show what the carrier actually said ("Held at customs"). `null` until the first successful poll. |
| `eta` | Carrier-estimated delivery. `null` when the carrier gives none. |
| `delivered_at` | When the package landed, stamped by whichever path saw it first (a delivered email or a poll) and never overwritten after. `null` until delivered. |
| `last_polled_at` | Last poll **attempt**, success or permanent failure. `null` means never polled, which is also true of every row when no carrier is configured. |
| `poll_failures` | Consecutive permanent failures. Surfaced because it is evidence about the number itself, not just about the poll. |

The agent door's `get_shipments` gained `eta` and `carrier_status_raw`, and only
those two. `delivered_at`, `last_polled_at` and `poll_failures` stay on the human
door: they are operator diagnostics about our polling, not facts about the
package an agent should be relaying.

`status` stays the five-rung ladder (`ordered`, `shipped`, `out_for_delivery`,
`delivered`, `exception`). The carrier is ground truth for it, with one
exception: `delivered` is terminal and a later carrier reading never walks it
back.

### Metrics

With `SQUELCH_METRICS_BIND` set, three series describe polling:

| Metric | What it is |
|---|---|
| `squelchd_carrier_poll_total{carrier,outcome}` | Every poll, by carrier (`ups`, `fedex`, `usps`, `dhl`) and outcome (`ok`, `not_found`, `rate_limited`, `auth`, `transient`). All 20 series are always exported, including carriers you never configured, so a `rate(...)` on a newly-configured carrier has a history to work from. |
| `squelchd_shipments_advanced_by_poll_total` | Polls that **moved** a package's status, as opposed to confirming it. This is the number that says the feature is earning its quota. |
| `squelchd_shipment_poll_last_success_timestamp_seconds` | Unix time of the last poll any carrier answered. 0 if none ever has. |

Worth alerting on:

```promql
# Configured, and yet nothing has answered in six hours.
time() - squelchd_shipment_poll_last_success_timestamp_seconds > 21600

# A credential has gone bad. Auth failures are never transient here.
increase(squelchd_carrier_poll_total{outcome="auth"}[1h]) > 0
```

The freshness alert fires on a daemon that has never had a successful poll, since
the stamp is 0 until one lands. On a daemon with no carriers configured that is
noise, so scope it to the deployments you actually gave credentials to.

`rate_limited` climbing on DHL or USPS means the budget is not holding, which
usually means the daemon is restarting often enough to reset the in-memory
window. `not_found` climbing across carriers means the detector is minting
numbers that are not tracking numbers, which is a detection bug rather than a
polling one.

### Amazon stays email-only, by design

Amazon Logistics `TBA…` numbers are detected, stored, and tracked from mail like
anything else, but they are never polled and they carry no `tracking_url`. There
is no public Amazon tracking API keyed by a TBA number and no public URL that
resolves one: that tracking lives behind an Amazon account login, in the orders
page. The poller's carrier list is `ups`, `usps`, `fedex`, `dhl` for exactly this
reason, and `unknown`-carrier rows are excluded on the same grounds. An Amazon
package's status is therefore whatever the last Amazon email said, permanently.
That is not a gap waiting to be filled; there is nothing to fill it with.

## Identity: which package is this?

### Tracking number first, order reference second

The `shipments` table's identity is the **tracking number**: it is the unique key
per account, and it is what dedupes the four emails a single package generates.

An `order_ref` (the retailer's own identifier, like `112-3456789-1234567`) is
recorded alongside it when the mail carries one. It is not a dedupe key. It is
the join back to the third piece:

**`shipment_orders`, the staging table.** An order confirmation usually arrives
days before the ship notice, and it carries no tracking number at all, so it
cannot live in `shipments`. It is staged here instead, keyed by
`(account, order_ref)`. When the ship notice lands carrying both the order
reference and a real number, the staged row is promoted: the shipment absorbs its
item name (only if the shipment has none of its own) and the staged row is
deleted.

Staged orders are not currently served on either door. They exist so that the
shipment you eventually see is named "Anker USB-C charger" instead of "your
package", and so that a future orders view has something to read.

### The detector, tightened

The regex detector still runs at ingest on every non-sealed inbound mail, and
still handles the self-identifying shapes outright: UPS `1Z…`, Amazon `TBA…`, and
USPS IMpb numbers (a long digit run starting 92, 93 or 94). Their prefixes
identify them and they need no help.

The ambiguous shapes are the bare digit runs: 12, 15 or 20 digits (FedEx), 20 to
22 digits (USPS without an IMpb prefix), and 10 to 11 digits (DHL). Those are now
accepted only when at least one of these holds:

- a tracking label sits near the number (roughly 80 characters before or 40
  after), matching "tracking number", "tracking #", "track", and friends;
- the number is embedded in a carrier tracking URL (`tracknum=`, `trknbr=`,
  `tLabels=`, `tracking-id=`, `TrackConfirmAction`);
- the mail came **from** `ups.com`, `usps.com`, `fedex.com` or `dhl.com` (exact
  domain or a subdomain, so a look-alike like `ups.com.example.net` does not
  count).

And they are refused outright, whatever else the mail says, when:

- the number sits inside a URL containing `/itm/`, `/ord/` or `ebay.`;
- the number is immediately preceded by an item or order label ("Item 234567890123",
  "order #987654321098").

The refusal wins over the context. A mail titled "Tracking details for your USPS
shipment" whose body says "Item 234567890123 was packed" produces no shipment,
which is the correct answer and was not the old one.

Return and refund mail is excluded ahead of all of this, even when it also
discusses the original inbound delivery.

### The shipments extractor

The regex cannot rescue an order confirmation that contains no number, and it
cannot name what was bought. That is the extractor's job.

**Trigger.** At ingest, every non-sealed, non-sent mail is tested for a *loose*
shipping signal: shipping, tracking, delivery, package, parcel, carrier, in
transit, and so on, with no tracking number required. It is deliberately
high-recall. A hit stamps `triage.ship_extract_model = 'pending'`, and that
column is the queue.

**Queue.** The shipments extractor has its own queue, separate from the
category-routed banking and marketing extractors, because it routes on that
trigger rather than on a triage category. It drains newest-first, up to
`stage1.batch_per_cycle` rows per sync cycle, and skips (without a model call)
anything older than `carriers.max_age_days`. That horizon is deliberately the
carrier poller's, not the usual one-week extractor horizon: a package ordered
three weeks ago is still in flight, and one horizon for the whole feature means
the poller can never end up chasing a row the extractor skipped unread.

**What it returns.** A closed schema: `is_shipment`, `tracking_number`,
`order_ref`, `item_name`, `carrier`, `status`. It does not produce an ETA; ETAs
come only from carrier polls. Every field is validated before it is stored, and
a model verdict can only ever delete an *ambiguous*-shaped row fed by the same
message, so a false negative cannot destroy a real `1Z…` package.

### What extraction costs

- **One model call per email carrying a shipping signal.** Not per package, and
  not per email overall.
- **On the Stage-1 model** (default `claude-haiku-4-5`), with a 3000-character
  body budget spent on the regions of the mail that actually carry numbers.
- **Against the shared Stage-1 daily cap.** `stage1.global_daily_cap` (default
  1000) is one counter, shared by Stage-1 triage itself and by all three
  extractors. Shipments extraction does not get its own allowance; it competes
  with triage for that one.
- **Billed to its own ledger line**, `extract_shipments`, so you can see what it
  costs separately from `extract_banking`, `extract_marketing` and Stage-1
  itself. It appears as its own category in `GET /client/usage` (and therefore in
  Passband's usage view) as soon as the ledger has seen one call.

When the shared cap is exhausted, remaining rows stay `pending`, unstamped, and
are retried on later cycles. Nothing is lost and nothing is double-billed. You
get one warning per day:

```
squelch: stage-1 global daily budget exhausted (1000/1000); shipment rows stay queued
```

### Turning extraction off

**There is no switch for this, and that is a gap worth stating plainly.** No
config key, env var or endpoint disables the shipments extractor specifically.
The two available levers are both blunt:

1. **Configure no LLM key at all.** With no `ANTHROPIC_API_KEY` /
   `OPENAI_API_KEY` / `SQUELCH_STAGE2_API_KEY`, the whole extract pass is a
   silent no-op. This also turns off Stage-1 refinement, Stage-2 triage, and the
   banking and marketing extractors. It is the "no LLM spend at all" setting, not
   a shipments setting.
2. **Lower the shared Stage-1 daily cap** (`stage1.global_daily_cap`,
   `SQUELCH_STAGE1_GLOBAL_DAILY_CAP`, or `stage1_global_daily_cap` via
   `POST /client/triage-config` and Passband's Settings, which takes effect
   without a restart). This throttles shipments extraction and Stage-1 triage and
   the other two extractors, identically, because they share the counter.

**A cap of 0 is not available.** Valid caps are 1 to 100000: the config loader
clamps 0 up to 1 with a warning, and the endpoint rejects it with a 400. That is
deliberate (a cap of 0 would silently block every row of every LLM pass forever),
but it does mean the floor you can reach without pulling the API key is one
shared call per day, not zero.

So: if you want mail triage but not shipment extraction, today you cannot have
it. If someone needs that, the honest fix is a per-extractor toggle, and it does
not exist yet.

## First start after upgrading

Three one-shots run on the first daemon start with this build, and then never
again.

1. **Schema migration.** The poll-state columns, `shipments.order_ref`, the
   `shipment_orders` table and their indexes are added in place. Additive and
   silent.
2. **Phantom cleanup.** Every existing `shipments` row is re-judged against its
   own feeder email under the tightened detector, and rows the detector no longer
   produces are deleted. Rows written by an older daemon that have no feeder
   message are left alone: there is no evidence to re-judge them on. It logs a
   count and never a tracking number:

   ```
   squelchd: shipment re-detect removed 14 stale shipment row(s)
   ```

   A failure here just retries on the next start, and the done-flag is only set
   after a successful pass.
3. **Trigger backfill.** Up to **500** of the newest inbound messages from the
   last **30 days** are tested for the shipping signal and queued for the
   extractor, so the feature starts with the packages already in flight rather
   than only with tomorrow's mail. It is silent, and it is a real one-time LLM
   cost: up to 500 Stage-1 calls, drained over subsequent sync cycles and bounded
   the whole way by the shared Stage-1 daily cap. If that matters on your budget,
   lower the cap before the first start.

The same migration also clears stale `skip-no-extractor` markers left by an older
dispatch bug, putting those banking and marketing rows back in their queue. That
is unrelated to shipments, but it lands in the same start and it too spends
Stage-1 calls.
