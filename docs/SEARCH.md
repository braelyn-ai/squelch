# Search: keyword, meaning, and the agent lane

Status: designed 2026-09-02; the build is the waves in §7. Wave 0 (the `s`
key) ships with this document. This is the design record for making search
find the mail you mean, not just the mail that contains your words, and it
starts from a query that the current search handled worse than a human did.

## 1. What the motivating query taught us

The query was "abstract conference wifi password". The reader knew the email
existed; the search panel did not surface anything useful; the ⌘K agent found
it in one pass. Run against the live mailbox on 2026-09-02 (1,526 messages,
1,561 FTS rows, daemon on 127.0.0.1:8848), the three legs do this:

| mode | hits | where the right email lands |
|---|---|---|
| `keyword` | **0** | nowhere |
| `semantic` | 10 | #1, but see below |
| `hybrid` (the app's default) | 10 | #1 under `recent`, #3 under `best_match` |

The right email is #1542, "Abstract is today." from the conference. It wins
the vector leg **only because the reader typed the conference's name**: the
subject and sender are "Abstract", so every Abstract email clusters at the
top. Drop that one word and it is gone: `semantic` for "conference wifi
password" returns Telnyx IP subnets, a Cloudflare invite and a Wispr
newsletter, and #1542 is not in the top ten.

The keyword leg returned nothing for a reason worth stating precisely. The
body of #1542 is 568 characters and reads, in the relevant part:

> Have your QR code ready when you get to SFJAZZ. Open the link below, scroll
> to Check-in, and show the code at the door. … **Wifi and the FAQ are on the
> same page.**

**The word "password" is not in the email. Neither is "conference".** FTS5
treats a bare query as an implicit AND of every token, so a query with one
word the document lacks matches nothing, however rare and decisive the other
words are. And no embedding model retrieves a word that is not in the text:
the mail does not contain the wifi password, it says where the wifi details
live. The agent won because it searched for a subset of the words, opened the
thread, read that sentence, and pointed at the agenda link. That is reasoning
over a result, not better retrieval.

So the query exposes two different failures, and they need two different
fixes:

1. **Lexical brittleness.** `abstract OR conference OR wifi OR password`,
   ranked by bm25, puts #1542 at **#1 in pure keyword mode** (its bm25 is
   -19.9; the next Abstract mail is -18.7; the password-reset noise starts at
   -7.7). "wifi" appears in exactly one message in the mailbox, so IDF alone
   does the work. The keyword leg's AND semantics threw that away. This is a
   one-line class of fix and it is §4.
2. **Questions whose answer is not spelled in the document.** "Where is the
   wifi password" is answered by an email that never says "password". No
   index fixes that; something has to read the candidate and reason. That is
   the agent lane, §6, and the classifier that decides when to start it, §5.

One more finding from the same run: a hit the vector leg found shows the
STORED snippet (the head of the message), because only keyword hits get an
FTS match window. For #1542 that is "ABSTRACT IS TODAY. Hi Braelyn, Abstract
is today. Doors are open at 9:00 AM…", which says nothing about wifi. Even
when the right row is #1, the panel gives the reader no reason to believe it.

## 2. The retrieval stack today

Facts, from the code, so the evaluation in §3 argues against what is there
rather than what one remembers being there.

- **Keyword leg** (`squelch-core/src/store/sqlite/search.rs`,
  `search_filtered` / `fts_recall`): `messages_fts` is `fts5(subject, body)`
  with the DEFAULT tokenizer, so `unicode61` with no stemming ("passwords"
  does not match "password") and no prefix matching. The reader's text is
  bound straight into `MATCH ?`, so it is an implicit AND, and a token FTS5
  cannot parse (a stray `"` or a trailing `-`) makes the whole MATCH invalid,
  which every caller reads as "no keyword hits" rather than an error. bm25 is
  unweighted: a term in the subject counts the same as one in the body.
  Recency is blended in SQL, multiplicatively (PR #135). Every hit query pairs
  `is_spam = 0` with the sealed guard (PR #178): spam is a structural
  exclusion, absent from every count and search rather than ranked last.
- **Semantic leg**: one sqlite-vec `vec0` row per non-sealed message,
  `bge-small-en-v1.5` fp32 at 384 dimensions via fastembed 5.17 over ONNX
  Runtime, embedding the subject plus the first 1,000 characters of the body
  (256 tokens). The query is embedded RAW: BGE v1.5's retrieval instruction
  ("Represent this sentence for searching relevant passages: ") is not
  prepended, and fastembed does not add it either. Brute-force KNN, which is
  the right call at this corpus size.
- **Hybrid**: Reciprocal Rank Fusion of the two lists plus an additive
  recency vote, top-`k` (`recall_k`, capped at 600), operators applied
  post-hoc, sealed rows excluded in every leg.
- **The wire** (`GET /client/search`): items, `match_kind`, `sort`,
  `next_cursor`. Nothing about WHY a hit matched or how each leg did, so the
  client cannot tell a confident keyword answer from a vector leg guessing.
- **The agent's `search_mail` tool** calls the same door with
  `mode=hybrid`. Every improvement to the door is an improvement to the agent
  lane's own searches, which is why §4 ships before §6.
- **Hosted memory**: the ONNX session is 85-90% of a tenant pod (300-500 MB;
  `docs/EMBED-SERVICE.md`). Anything that grows the model or adds a second
  one is a fleet decision, not a search decision.

## 3. Tooling evaluation

### 3.1 The constraints that decide it

- **Corpus size.** 1.5k messages here; a heavy user is tens of thousands, not
  millions. Nothing here is a scale problem. Every option below is judged on
  ranking quality for a small personal corpus with vocabulary mismatch, which
  is a different problem from web search.
- **Who sees the mail.** Self-host promises on-box search; the only remote
  party is the BYOK LLM the user chose. Hosted sends mail bodies to Google and
  to Anthropic (through Bifrost) and the privacy policy was rewritten this
  summer to say exactly that. A paid embedding or rerank API is a NEW
  processor: a policy change and a CASA scope change, not a config change.
- **Hosted memory.** See §2. A bigger embedder multiplies a number that
  already decides the tenant count.
- **No re-embed path.** `messages_missing_vectors` finds messages with NO
  vector. Changing the model means dropping `message_vecs` and backfilling
  the mailbox; changing the dimension also means editing the `FLOAT[384]`
  literal in `schema.sql`.

### 3.2 Open source, on the box

| option | fixes | costs | verdict |
|---|---|---|---|
| **FTS5 query construction**: quote each token, AND first and fall through to OR-ranked partial matches, `porter unicode61` tokenizer, subject weighted above body in `bm25()`, prefix on the trailing token as you type | §1's zero-hit failure outright; plurals; typing "wif" matching "wifi"; an unparseable query silently returning nothing | zero memory, one FTS rebuild on existing DBs (seconds at 100k rows) | **do, wave 1** |
| **BGE query instruction** on the query side only | short-query-to-passage retrieval, which is what a search box is; the model card asks for it | zero | **do, wave 1**, measured with the real-model e2e test before it stays |
| **Match window for vector hits**: the body sentence with the most query-term overlap, else the stored head | §1's "the right row with no reason to believe it" | zero | **do, wave 1** |
| **Diagnostics on the wire**: strict-keyword hit count, any-term hit count, per-term document frequency, the leg each hit came from | gives §5's classifier facts instead of guesses | zero | **do, wave 1** |
| **Chunked embedding**: N paragraph-window vectors per message, `vec0` keyed by chunk with `message_id` as a metadata column | text buried past the 1,000-character cut | N× ingest embedding time, N× vector rows, a `vec0` re-key | **later, on evidence.** The motivating email is 568 characters; the cut was not the problem. Revisit when a real miss is traced to it |
| **Same-size better embedder**: `snowflake-arctic-embed-s` (384-d, same footprint) | marginal retrieval gain at no memory cost | full backfill; benchmark deltas at this size are within noise on a personal corpus | **not now**; fold into the shared-session work in EMBED-SERVICE.md when the model becomes one fleet decision |
| **Bigger embedder**: `bge-base-en-v1.5` (768-d, ~3.5× memory), `nomic-embed-text-v1.5` (768-d, 8k context, ~550 MB) | nomic's long context would replace chunking | multiplies the number that sets the hosted tenant count; schema dimension change | **no** until embedding is one session per node |
| **Cross-encoder reranker** over the fused top 50: `bge-reranker-base` (~1.1 GB fp32), `jina-reranker-v1-turbo-en` (~150 MB), both in fastembed | sharper top-10 for question-shaped queries | a second ONNX session in every pod; jina is the only size hosted could carry, and only after EMBED-SERVICE | **no for hosted**; a self-host opt-in later if the agent lane leaves a gap |
| **Learned sparse (SPLADE++)** | vocabulary mismatch ("wifi" vs "wireless") without an LLM | 110M-parameter model, a sparse-vector table, a new fusion leg | **no**; the agent lane covers mismatch with a party we already use |
| **Tantivy / embedded Meilisearch / Typesense** | fuzzy, stemming, boosting, phrase | a second index to keep consistent with SQLite; FTS5 does everything we need at this scale | **no** |

### 3.3 Paid

| option | what it would buy | what it costs beyond money | verdict |
|---|---|---|---|
| **Voyage AI** (`voyage-3.5-lite` ~$0.02/M tokens, `voyage-3-large` ~$0.18/M; 32k context; list prices at writing) | the best embedding quality per dollar; would delete ONNX Runtime from hosted pods entirely, which is the whole EMBED-SERVICE.md problem | a new processor (Voyage is MongoDB's), so policy + CASA; mail bodies leave for a company the user has not heard of | **not now.** The one paid option with a real structural upside, and it is a HOSTED-ONLY upside. If hosted memory forces the embedder off the pod before the shared session lands, this is the candidate, routed through Bifrost if it proxies `/v1/embeddings` (verify), never on self-host by default |
| **OpenAI `text-embedding-3`**, **Gemini embedding** | same shape as Voyage, somewhat lower quality per dollar | same new-party problem | **no** |
| **Cohere `embed-v4` + `rerank-3.5`** (~$2 per 1k rerank calls) | the rerank API maps onto a real gap: a question over a small candidate set | a new party, and a per-search bill that scales with keystrokes | **no**; the agent lane spends that budget with a party we already use and gets reasoning, not just reordering |
| **Hosted search indexes**: Algolia, Elastic Cloud, Typesense Cloud, Meilisearch Cloud | operational ease | the whole mailbox in a third party's index; self-host users will not run Elastic | **hard no** |
| **LLM query expansion** (a Haiku rewrite into keyword variants, HyDE) | vocabulary mismatch | an LLM call per settled query | **do not build separately**: it is the agent lane's first turn, and building it alone pays the round trip without getting the reading |

### 3.4 Recommendation

Spend nothing on retrieval infrastructure. Ship the free FTS and embedding
fixes first (wave 1): they fix §1's zero-hit class, and they raise the floor
under the agent lane's own `search_mail`. Then spend LLM budget, which we
already spend and already disclose, on the agent lane (wave 2): that is where
"the answer is not spelled in the document" gets solved, and it is the only
option in either table that would have answered the motivating query on its
merits rather than on the reader having typed the right proper noun.

Revisit paid embeddings only when hosted memory forces the embedder off the
pod, and then as a hosted-only decision under a policy change.

## 4. Wave 1: the keyword leg stops being brittle

All in `squelch-core`, all covered by `store/sqlite/tests/search.rs`, and
every ranking change is mutation-tested against the live mailbox (the recency
PR's lesson: green proves nothing on a ranking change).

1. **Tokenise and quote.** Split the reader's text on whitespace, drop FTS5
   syntax characters, wrap every token in double quotes. The reader's text is
   never again parsed as FTS5 syntax, so the "malformed MATCH means zero
   hits" path becomes unreachable from a search box. Operators stay
   `parse_search_query`'s business and are lifted out before this.
2. **AND, then OR.** Run the strict all-terms match first. When it returns
   fewer than the page, append the any-term match (`"a" OR "b" OR "c"`)
   ranked by bm25, deduplicated, after the strict hits. Exact matches stay on
   top; partial matches become findable instead of absent. Both `fts_recall`
   (the hybrid leg's keyword input) and `search_filtered` (keyword mode) use
   the same builder so the two modes cannot disagree about what a query
   means. Pagination on the keyword leg stays exact: the OR page is offset by
   the strict count.
3. **Stem.** `tokenize = 'porter unicode61'`. `migrate.rs` detects the old
   tokenizer from the `CREATE VIRTUAL TABLE` text in `sqlite_master`, drops
   and recreates `messages_fts`, and refills it from `messages(subject,
   body)` in one transaction. Idempotent; a fresh DB gets it from
   `schema.sql`.
4. **Weight the subject.** `bm25(messages_fts, 4.0, 1.0)`. A word in the
   subject is the sender telling you what the mail is about.
5. **Prefix the trailing token** while the query is being typed: the last
   token gets `*` when the door is asked with `partial=1`, which the panel
   sends from its debounced fetch and the agent never does.
6. **Query instruction** for BGE on the query side only. Corpus vectors are
   untouched, so this is a search-time change with no backfill. The
   real-model e2e test (`embed_e2e_real_model_ranks_relevant_first`) grows a
   short-query case, and the instruction stays only if it does not regress
   the existing cases.
7. **A match window for every hit.** A hit the keyword leg did not produce
   gets a sentence from the body chosen by query-term overlap (case-folded,
   stemmed), falling back to the stored head only when no term appears at
   all. Same size as the FTS window.
8. **Diagnostics** on `SearchPage`: `strict_hits` (all terms), `any_hits`
   (any term), `terms: [{text, df}]`, and per-item `legs: ["keyword",
   "vector"]`. Additive fields; the iOS client ignores what it does not
   decode. Every count is account-scoped and excludes sealed AND spam rows
   exactly as the hit queries do: a document frequency that counted sealed
   mail would be an oracle for what sealed mail contains.

## 5. Keyword or question: the classifier

The iOS surface counts spaces (`MobileSearchView.swift`: four spaces, five
words, and the field becomes the agent's). The comment explains the
principle and the principle survives: a rule "wrong in a way anybody can see
and fix by deleting a word" beats a model deciding which door you meant for a
round trip, on your key, wrongly in ways nobody can predict. The Mac gets a
better rule, not a model.

Two kinds of signal, both free and both explainable:

- **Shape of the query.** Starts with a question word (what, when, where,
  who, which, how, did, do, does, is, was, can); ends in `?`; carries a
  first-person marker ("my", "I", "me"); or has five or more words. Any one
  of these and the query is a question.
- **What retrieval did with it** (§4.8's diagnostics). Three or more terms
  and `strict_hits == 0` means the reader's words do not co-occur in any one
  message, which is exactly the shape of "abstract conference wifi
  password": the words are right, the mail just does not use all of them. A
  query whose every hit is vector-only says the same thing more weakly.

A query is **deeper** when it is question-shaped, or when it has three or
more terms and no strict keyword hit. A query carrying an operator (`from:`,
`after:`, `before:`) is **never** deeper: operators are structured intent and
the reader is already speaking the index's language. One or two plain words
are never deeper either: "wifi" is a lookup, and its one hit is on screen.

The decision is made once per settled query (after the panel's 220 ms
debounce and the fetch), so the rule never fires mid-keystroke, and it is
shown: the deeper band (§6.4) says which signal started it.

## 6. Wave 2: the agent lane

### 6.1 What it is

A second `AssistantSession`, owned by `SearchSession`, configured as a
**lane**: a fixed subset of tools, its own system prompt, its own model
default, and two capabilities the ⌘K chat does not have, refinement and
pause. `AssistantSession` grows a `Lane` configuration rather than being
forked: the streaming loop, the echo-every-tool_use invariant, the citation
bookkeeping and the rollback-on-failure are the hard parts, and they are
already right.

- **Tools**: `search_mail`, `get_thread`, `search_contacts`, `get_records`,
  `show_emails`. READ and SHOW only. No acting tool, so no confirm card can
  ever appear inside the search panel, and the lane needs no `ActionGate`.
- **Model**: Haiku by default, its own preference, independent of ⌘K's. A
  search lane runs on every deeper query; the chat runs when asked.
- **Prompt**: find, do not converse. The answer is `show_emails` cards plus
  at most one line saying what the reader will find and where ("the wifi
  details are on the agenda page linked from this email"). It is told the
  reader's exact words, the local search's top hits (thread ids and
  subjects, so it does not repeat the search that already ran), and that
  every refinement it receives is the reader narrowing the same need, not a
  new question. The trust block is the chat's, verbatim: mail is data.
- **Turn budget**: the chat's eight.

### 6.2 Refinement

The reader keeps typing; the local list keeps refetching as today; the lane
does NOT restart. Each settled query after the first is a refinement and
reaches the lane one of two ways:

- **Idle lane** (a turn has finished): the refinement is the next user turn
  in the same conversation.
- **Running lane**: the refinement is queued and delivered at the next
  tool-result boundary, as a text block appended to the same user message
  that carries the `tool_result` blocks. The provider accepts text after
  results in one user turn, and that boundary is the only place a message
  can be inserted without breaking the tool_use/tool_result pairing the
  chat's comments are so insistent about. If no boundary comes before the
  turn ends, the idle rule applies.

Refinements coalesce: only the newest pending one is delivered. "abstract
conf" then "abstract conference wifi" is one refinement, the second. Twelve
refinements in one lane and it is reset and started fresh on the next: a
conversation that long is a search that changed subject.

### 6.3 Pause and hold

Closing the panel pauses the lane; nothing is lost. There is no such thing as
pausing an HTTP stream, so the pause is at the loop boundary: the in-flight
model turn is allowed to finish (it is bounded by `max_tokens` and is what
has already been paid for), no tool runs, no next request is made, and the
session sits with its history, transcript and cards intact. `running` stays
true so a reopened panel can pick up without a restart.

Reopening the panel resumes at that boundary. The query the reader comes
back to is the one that was there, and if they change it, that is a
refinement (§6.2). Two things reset the lane instead: an explicit seed
(`f` on a row opens `from:<address>`, which is a different search), and the
panel's own "new search" control. Nothing else does, and there is no timeout:
holding a paused conversation costs kilobytes.

### 6.4 Where it shows

The strip (460 pt) is too narrow for two columns, so in the strip the lane is
a **band above the hits**, collapsible, mounted only once a query has been
judged deeper. It carries one status line while the lane works (the tool
chips reduced to their summaries: searching "abstract wifi", reading "Abstract
is today."), then the email cards and the one-line note. Paused, it shows the
pause glyph and the last state. Expanded (Enter in the bar), the band becomes
the **right column** beside the results, which is the side-by-side the
request asked for. Cards open the thread exactly as AskBar's do.

The band names its trigger in small type ("no email has all four words") so a
reader who did not want the lane can see why it ran and shorten the query.

### 6.5 Cost and consent

On self-host the lane runs on the user's own key, on hosted on the relay's
credential. Either way it is spend the reader did not tap for, so it is a
setting, `Deeper search`: **automatic** (the classifier starts it), **on
request** (the band offers a button, iOS-style), **off**. Default automatic.
Every lane start is one `search_deeper_started` event carrying the trigger
kind and the model tier, added to `Analytics.allowedEvents` and its strings
to `allowedStrings`, because both are closed sets and a stray string is a
fatal assert in debug builds.

### 6.6 Security

Same door as the chat: `/client/*`, sealed mail structurally absent, and the
lane has no tool that writes. The reader's query and the refinements are the
reader's words and go to the model as the user turn; the local hits go in as
ids and subjects behind the same markers the chat uses for a pinned subject.
The one-line note renders as plain text, not markdown, and any link in a card
is a thread id, never a URL from the mail.

## 7. Waves

| wave | what | proof |
|---|---|---|
| 0 | remove `s` (search this sender) from the reader; `/` then `from:` remains | app builds; this document |
| 1 | §4: FTS query builder, AND-then-OR, porter + migration, subject weight, trailing prefix, BGE query instruction, match window for every hit, wire diagnostics | `search.rs` tests for each; the live-mailbox mutation run recorded in the PR; `embed_e2e_*` for the instruction |
| 2 | §5 + §6: `Lane` on `AssistantSession`, refinement injection, pause/resume, the classifier, the band and column, the setting, analytics | a swiftc test suite for the classifier and for the refinement/pause state machine (pure types, no network); seen on screen against the live daemon with the motivating query |
| 3 | `docs/CHANGELOG.md` via `ReleaseNotes.swift`; this document's status line | `make-changelog.sh --check` |

iOS keeps its space-counting rule for now. The lane is a `SearchSession`
concern and ports once the Mac shape has been used for a week.
