# Google OAuth verification

Passband requests two **restricted** Gmail scopes. Until Google verifies the
project, consent is capped at 100 Google accounts, forever, and nothing else
moves that number. This is the record of what the submission says, what it is
blocked on, and what a reviewer will find if they look.

`docs/HOSTED.md` decided the shape and is not relitigated here: **one Cloud
project, one consent screen, two OAuth clients** (Desktop for self-host, Web for
hosted), therefore one verification and one security assessment.

## The three gates, in order

| Gate | What it is | Blocks |
|---|---|---|
| **1. Brand verification** | Automated + human check of app name, logo, home page, privacy policy, domain ownership. 2-3 business days. | Everything below |
| **2. Restricted scope verification** | Scope declarations, justifications, permitted application type, demo video. Human review. | Consent above 100 users |
| **3. CASA Tier 2** | Independent security assessment by an App Defense Alliance authorized lab. The long pole, and the only gate with a bill attached. Repeats **every 12 months**. | Final approval |

Total realistic lead time from a clean submission: **4-12 weeks**.

### Why CASA is not optional for us

Google requires the assessment of any app that "has the ability to access data
from or through a third-party server." Self-host alone would very likely have
escaped it: a Desktop client, loopback consent, mail that never leaves the
user's disk. **Hosted is what triggers it.** Tenant pods on `carrier` hold the
mail, the control plane holds sealed refresh tokens, and `bifrost.passband.app`
carries message content in transit. One project means the Desktop client
inherits that obligation. That tradeoff was made deliberately; this is the bill.

## Blockers

Ordered by what stops the submission soonest.

- [ ] **The consent screen still says "Squelch."** It must say **Passband** —
      the name on the home page, in the app, and on the App Store listing.
      A mismatch fails brand verification on the first automated pass.
      (`docs/HOSTED.md`, "Naming", has owed this since 2026-08-03.)
- [ ] **Verify `passband.app` in Google Search Console**, as an owner, from the
      same Google account that owns the Cloud project. Brand verification checks
      the top private domain of every URL on the consent screen.
- [ ] **Declare `https://passband.app/about` as the application home page** —
      not `https://passband.app`. The React homepage serves a 1,328-byte empty
      `#root` with no functional description and no privacy link, which is
      exactly what the automated home page check looks for. `/about` is static
      HTML (added 2026-08-25) and carries the description, the scope table, and
      the policy links without executing a bundle.
- [ ] **Confirm what Bifrost retains.** The gateway's `/app/data` volume is
      documented as "governance state: virtual keys, budgets, spend," but
      nothing in this repo establishes whether request and response *bodies* are
      persisted. If they are, Gmail message content is sitting in a Railway
      volume, undisclosed, and an assessor will find it. Turn body logging off,
      or disclose it in the privacy policy and scope it into CASA. **Do this
      before the assessment, not during.**
- [ ] **`/mcp` bearer auth** (`deploy/hosted/PRODUCTION.md`). An unauthenticated
      agent door is not something to be explaining to an assessor. Tenant
      Ingresses currently route around it; make that structural, or build the
      auth.

Cleared as of 2026-08-25:

- [x] Privacy policy rewritten to cover both tiers. The previous version claimed
      "we do not run servers that receive, store, or can access your email or
      your Google credentials," which stopped being true the day hosted shipped,
      and described LLM processing as bring-your-own-key only, which is false for
      hosted tenants routed through our own Anthropic account.
- [x] Terms of service extended to hosted accounts.
- [x] `/about` written and routed.

## What gets submitted

### Permitted application type

**Email client.** Passband is a mail client for macOS and iOS; the Gmail
mailbox is the entire product surface. No other type on Google's list applies.

### Scope declarations

Declare exactly these on the Data Access page. Anything not on this list is a
finding.

| Scope | Class |
|---|---|
| `https://www.googleapis.com/auth/gmail.readonly` | Restricted |
| `https://www.googleapis.com/auth/gmail.modify` | Restricted |
| `https://www.googleapis.com/auth/gmail.send` | Sensitive |
| `https://www.googleapis.com/auth/userinfo.email` | Sensitive |
| `openid` | — |

The constants are pinned in `squelch-core/src/config.rs` and re-pinned in
`squelch-broker/src/validate.rs`; the signup floor is tested in
`squelch-control/tests/signup_flow.rs`. Keep this table and those constants in
agreement.

### Justifications

Paste these into "describe how you will use the restricted scopes and why more
limited scopes aren't sufficient." Every one of them names the narrower scope
Google will ask about and says why it does not work, because the rejection is
otherwise automatic.

**`gmail.readonly`**

> Passband is an email client. It reads the user's messages to display them,
> thread them, and search them, and to run the triage that is the product's
> reason to exist: each message is scored so that the inbox holds only what
> needs attention and the rest is filed. Triage reads the message body, not just
> its headers, because sender and subject alone cannot distinguish a receipt
> from a bill, a newsletter from a personal note, or a routine notification from
> an account-security alert. The same body text is what the app extracts
> deadlines, package tracking numbers, and one-time verification codes from and
> surfaces as cards.
>
> `gmail.metadata` is not sufficient: it returns headers and labels with no
> body and no snippet. An email client that cannot render a message is not an
> email client, and every triage and extraction feature above would be
> impossible. `gmail.addons.current.message.readonly` is not applicable, as
> Passband is a native macOS and iOS application, not a Gmail add-on.

**`gmail.modify`**

> Passband writes back exactly the decisions the user or their own triage rules
> made: archive a message (remove the `INBOX` label), add or remove a user
> label, mark read or unread, and return a thread to the inbox at a time the
> user asked for. These are `users.messages.modify` calls and there is no
> narrower scope that permits them.
>
> `gmail.labels` is not sufficient: it governs label *definitions* — creating,
> renaming, and deleting labels — and cannot apply or remove a label on a
> message, which is the only thing Passband does with it. Archiving in
> particular is the removal of the `INBOX` label from a message and is
> unreachable without `gmail.modify`.
>
> Passband deliberately does **not** request `https://mail.google.com/`, which
> would additionally grant permanent deletion and full IMAP/SMTP access. It
> never deletes a message and has no code path that can. It also does not
> request any `gmail.settings` scope.

**`gmail.send`**

> Passband sends the mail the user writes in it: replies, forwards, new
> messages, and invitations the user chooses to share from their own address.
> Sending is always user-initiated from the composer.
>
> `gmail.send` is already the narrowest scope that can send. `gmail.compose` is
> broader, not narrower — it additionally grants create, update, and delete over
> the user's drafts, which Passband does not need and does not want. This is the
> minimal choice, not a fallback from one.

### Demo video

Unlisted on YouTube. Must be in English, and must show all four of: the consent
flow a real user sees, the **App Name** rendered correctly on the consent
screen, the **OAuth client ID visible in the browser address bar** during
consent, and each requested scope actually doing something in the app.

Shot list. Do not cut between the address bar and the consent screen — the
reviewer needs one continuous frame proving that client ID granted that consent.

1. **Home page.** `passband.app/about` in a browser. Read the first line aloud.
   Establishes what the app is and that the name matches.
2. **Hosted consent (Web client).** Start at `signup.passband.app`, click
   through to Google. Hold on the consent screen for a slow five seconds with
   the address bar in frame and legible: the URL must show `client_id=...` and
   the screen must show **Passband**. Scroll the consent screen so all three
   Gmail permissions are visible. Approve.
3. **Self-host consent (Desktop client).** Run `squelchd auth` in a terminal,
   let it open the browser, and hold the same shot: address bar with the second
   client's `client_id`, consent screen showing **Passband**. Approve. This is
   the second client under the same project and the reviewer should see it
   exists rather than discover it.
4. **`gmail.readonly` in use.** The app after first sync: the triaged inbox,
   open a thread and scroll it, run a search that returns results, show an
   extracted card (deadline or shipment) next to the message it came from.
5. **`gmail.modify` in use.** Archive a message and show it leave the inbox.
   Apply a label and show it on the message. Park a thread for later and show it
   return. Then show the same messages in **Gmail's own web interface** so the
   reviewer sees the change actually landed on Google's side.
6. **`gmail.send` in use.** Compose a message, send it, and show it arrive in
   the recipient's mailbox and in the sender's Gmail Sent folder.
7. **Revocation.** `myaccount.google.com/permissions`, showing Passband listed
   and revocable. Not required, but it answers the question a reviewer is paid
   to ask.

### Documentation links

Up to three. Use:

1. `https://passband.app/privacy`
2. `https://github.com/braelyn-ai/squelch/blob/main/docs/SECURITY.md`
3. `https://passband.app/about`

## CASA Tier 2

Engage an App Defense Alliance authorized lab once scope verification is under
way; the assessment and Google's review can overlap, and the LOA date starts the
12-month clock, so do not start it early for nothing.

### What it costs: get quotes, do not trust a number

**Do not budget this off a blog post, including the numbers below.** The cheap
figures in circulation (TAC Security's ~$540 entry price is the one everybody
quotes) are for the **self-scan** path, where the developer runs the AST tools
and a lab only validates the output without touching code or infrastructure.
The ADA's own Tier 2 page now carries a banner reading "The CASA self scanning
process is deprecated," and its assurance-levels page describes both AL1 and AL2
as *Lab Tested - Lab Verified*. The cheap tier is the one being retired, so
pricing anchored to it is pricing the past.

Reported lab quotes span roughly **$500 to $4,500**, with $900-$1,500 typical
and some labs starting at $1,200+. TAC Security is the only lab Google labels a
*preferred* partner, with pricing it negotiated for developers, so it is worth a
quote even if it is not the one we take.

Assume the **upper** half of that range, because labs price on complexity and
Passband is not one web app. The scope below is five surfaces plus a
multi-tenant cluster; a quote at the bottom of the range means the lab has
misunderstood what it is assessing, which is a worse problem than a big invoice.
Get three quotes against the scope table, in writing, before committing.

**Assessment scope is everything that touches Gmail data**, which is more than
the daemon:

| Surface | Why it is in scope |
|---|---|
| `signup.passband.app` (`squelch-control`) | Runs the OAuth exchange, seals the refresh token |
| `*.passband.email` tenant vhosts | Serve `/client`, `/console`, `/t` off the tenant's own mail |
| `bifrost.passband.app` | Message content in transit to Anthropic |
| The push/read-receipt relay | Opaque tokens only, but it is our surface |
| `carrier` (Hetzner) and the k3s cluster | Where the mail and the sealed tokens rest |

Have ready before the DAST scan, because the questionnaire asks for all of it:
the threat model (`docs/SECURITY.md`), the tenant isolation story (namespace,
NetworkPolicy, per-tenant age identity), secret custody
(`deploy/hosted/PRODUCTION.md`, "Backups" and the custody table), the
encryption-at-rest posture (k3s `--secrets-encryption`, age-encrypted Litestream
to R2), and the deletion path.

## What a reviewer or assessor will poke at

Answers we should have written down before we are asked, not after.

- **Embedded client credentials in a public Docker image.** The self-host
  Desktop client's id and secret ship in `ghcr.io/braelyn-ai/squelchd`. This is
  the sanctioned installed-app model — Google treats Desktop client secrets as
  non-confidential and it is what rclone and Thunderbird do — and
  `docs/BROKER.md` sets out why the alternatives are worse. Be ready to say so
  in one paragraph.
- **Message content reaching Anthropic.** Disclosed in the privacy policy for
  both tiers. The load-bearing facts: no training on the content,
  security-sensitive mail is detected deterministically and never sent to a
  model at all, and self-host requires the user's own key.
- **Why both a Desktop and a Web client.** A refresh token is bound to the
  client that minted it; routing self-host refreshes through our infra would put
  our uptime in the path of every hourly refresh and every token in our reach.
  `docs/HOSTED.md`, "OAuth architecture."
- **The 100-user cap.** Know the current consented-account count before
  submitting, and do not cross it while the review is open.

## Annual renewal

Restricted scope access lapses unless the app is re-verified and re-assessed
within 12 months of the assessor's Letter of Assessment. Google emails a
reminder; do not rely on it. Put the LOA date here when it arrives, and a
calendar reminder at LOA + 9 months, because a lapse is a fleet-wide outage that
looks like every tenant's token going bad at once.

**LOA date:** not yet issued.
