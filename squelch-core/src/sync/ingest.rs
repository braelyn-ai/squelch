//! The ingest pipeline: raw RFC822 bytes -> parsed -> flattened text ->
//! seal-first triage -> a [`TriagedMessage`] ready for an atomic store write.
//!
//! ORDERING IS A SECURITY INVARIANT: parse/flatten, then seal detection FIRST
//! (sealed mail is `sensitivity='sealed'`, importance 0, and never runs Stage-1 or
//! reaches any LLM), and only then, for non-sealed mail, contacts/rules -> Stage-1.
//! Network-free by design so it is testable against fixture bytes.

use crate::config::Stage1Config;
use crate::store::{SqliteStore, Store, TriagedMessage};
use crate::sync::html::sanitize_email_html;
use crate::triage::calendar;
use crate::triage::receipt;
use crate::triage::seal::{self, SealInput};
use crate::triage::shipment;
use crate::triage::stage1_with_config;
use crate::types::{AccountId, AttachmentInfo, FieldReasons, NewMessage, Sensitivity, Tier};
use chrono::{DateTime, Utc};
use mail_parser::{Address, MessageParser, MimeHeaders};

/// The raw identity/metadata the transport supplies alongside the RFC822 body.
/// When the native Gmail ids are absent the pipeline falls back to a header-derived
/// thread key (see [`fallback_thread_id`]).
#[derive(Debug, Clone)]
pub struct RawFetched {
    pub account_id: AccountId,
    /// Stable per-account message id (Gmail `message.id`, or a Message-ID hash
    /// fallback when absent).
    pub gmail_msg_id: String,
    /// Gmail `message.threadId` when available; otherwise a header-derived key.
    pub gmail_thread_id: Option<String>,
    /// Full RFC822 bytes (from `format=raw`), base64url-decoded.
    pub raw: Vec<u8>,
    /// Date fallback if the message lacks a parseable Date header.
    pub internal_date: Option<DateTime<Utc>>,
    /// Whether this came from the Sent mailbox (seeds the contacts table).
    pub is_sent: bool,
    /// The account's own email, compared lower-cased so the user's own address can
    /// NEVER become a contact (contacts come from Sent mail's To/Cc, not From). May
    /// be empty when unknown — then only From is excluded, which on Sent mail is
    /// the account anyway.
    pub account_addr: String,
}

/// Crudely flatten HTML to text: drop tags, decode common entities, collapse
/// whitespace. The HTML-only fallback, so raw markup never reaches triage.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut tag_buf = String::new();
    // Inside a <style>/<script> block: that TEXT is code, not prose, and must not
    // land in the body fed to the triage models.
    let mut skip_block: Option<&'static str> = None;
    for c in html.chars() {
        match c {
            '<' => {
                in_tag = true;
                tag_buf.clear();
            }
            '>' => {
                in_tag = false;
                let name = tag_buf.trim().to_ascii_lowercase();
                let first = name.split_whitespace().next().unwrap_or("");
                match skip_block {
                    None => {
                        if first == "style" {
                            skip_block = Some("style");
                        } else if first == "script" {
                            skip_block = Some("script");
                        }
                    }
                    Some(k) => {
                        if first == format!("/{k}") {
                            skip_block = None;
                        }
                    }
                }
                out.push(' ');
            }
            _ if in_tag => tag_buf.push(c),
            _ if skip_block.is_some() => {}
            _ => out.push(c),
        }
    }
    let decoded = decode_entities(&out);
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Decode HTML entities into text: numeric forms (`&#8204;`, `&#x200b;`) and
/// the named ones email bodies actually use. Unknown entities pass through
/// literally (better a visible `&foo;` than eaten text). Zero-width characters
/// (ZWNJ/ZWJ/ZWSP/soft hyphen/BOM) decode to NOTHING — they are invisible
/// layout hacks (preheader padding), and as literal text they would pollute
/// FTS tokens and the LLM/embed input.
fn decode_entities(s: &str) -> String {
    fn named(name: &str) -> Option<&'static str> {
        Some(match name {
            "nbsp" => " ",
            "amp" => "&",
            "lt" => "<",
            "gt" => ">",
            "quot" => "\"",
            "apos" => "'",
            "zwnj" | "zwj" | "shy" => "",
            "mdash" => "\u{2014}",
            "ndash" => "\u{2013}",
            "lsquo" => "\u{2018}",
            "rsquo" => "\u{2019}",
            "ldquo" => "\u{201C}",
            "rdquo" => "\u{201D}",
            "hellip" => "\u{2026}",
            "copy" => "\u{A9}",
            "reg" => "\u{AE}",
            "trade" => "\u{2122}",
            "bull" => "\u{2022}",
            "middot" => "\u{B7}",
            _ => return None,
        })
    }
    /// Invisible layout characters that must not survive as text.
    fn zero_width(c: char) -> bool {
        matches!(
            c,
            '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{AD}' | '\u{FEFF}' | '\u{2060}'
        )
    }

    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let tail = &rest[amp..];
        // An entity is `&` + up to ~10 name/number chars + `;`. Anything else
        // is a literal ampersand.
        let semi = tail[1..].find(';').map(|i| i + 1).filter(|&i| i <= 12);
        let decoded = semi.and_then(|semi| {
            let name = &tail[1..semi];
            if let Some(rep) = named(name) {
                Some((rep.to_string(), semi + 1))
            } else if let Some(num) = name.strip_prefix('#') {
                let cp = match num.strip_prefix(['x', 'X']) {
                    Some(hex) => u32::from_str_radix(hex, 16).ok(),
                    None => num.parse::<u32>().ok(),
                };
                let c = cp.and_then(char::from_u32)?;
                Some((
                    if zero_width(c) {
                        String::new()
                    } else {
                        c.to_string()
                    },
                    semi + 1,
                ))
            } else {
                None
            }
        });
        match decoded {
            Some((rep, consumed)) => {
                out.push_str(&rep);
                rest = &tail[consumed..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    // Zero-width characters that arrived as RAW characters (an already-decoded
    // source) get the same treatment as their entity forms.
    if out.chars().any(zero_width) {
        out.retain(|c| !zero_width(c));
    }
    out
}

fn first_addr(addr: &Address) -> (String, Option<String>) {
    match addr.first() {
        Some(a) => (
            a.address().unwrap_or_default().to_string(),
            a.name().map(|n| n.to_string()),
        ),
        None => (String::new(), None),
    }
}

/// Collect every non-empty mailbox from an [`Address`] header (flat or grouped
/// lists) as `(address, display name)`. Sent mail's To/Cc drives two different
/// consumers off this one parse: the addresses become contacts, and the pairs
/// become the stored display recipients.
pub(crate) fn collect_mailboxes(addr: &Address, out: &mut Vec<(String, Option<String>)>) {
    for a in addr.iter() {
        if let Some(email) = a.address()
            && !email.is_empty()
        {
            let name = a
                .name()
                .map(|n| n.trim().to_string())
                .filter(|n| !n.is_empty());
            out.push((email.to_string(), name));
        }
    }
}

/// Render To/Cc mailboxes as the stored display string: `Name <addr>` when the
/// header carried a name, the bare address otherwise, comma-joined in header
/// order and deduplicated case-insensitively by address. A name holding a `,`
/// or `<` is quoted (`"Doe, John" <j@x.com>`) — the client splits this list on
/// unquoted commas, so a lastname-first address-book name must not read as two
/// recipients. Embedded `"` are dropped rather than escaped: the client's
/// splitter is quote-aware but not backslash-aware.
///
/// Deliberately NOT filtered the way contact seeding is: the contacts gates
/// (drop self, drop robot addresses) protect the "people I know" table, whereas
/// this field answers "who did this go to" and has to answer it faithfully —
/// including a note to self and including `support@`. `None` when nothing was
/// addressed at all, which leaves the column NULL.
pub(crate) fn format_recipients(mailboxes: &[(String, Option<String>)]) -> Option<String> {
    let mut seen: Vec<String> = Vec::new();
    let mut parts: Vec<String> = Vec::new();
    for (addr, name) in mailboxes {
        let addr = addr.trim();
        let lc = addr.to_ascii_lowercase();
        if addr.is_empty() || seen.contains(&lc) {
            continue;
        }
        seen.push(lc);
        parts.push(match name {
            Some(n) => {
                let n = n.replace('"', "");
                if n.contains(',') || n.contains('<') {
                    format!("\"{n}\" <{addr}>")
                } else {
                    format!("{n} <{addr}>")
                }
            }
            None => addr.to_string(),
        });
    }
    (!parts.is_empty()).then(|| parts.join(", "))
}

/// Heuristic: does `addr` look like a machine/robot address rather than a person?
/// Used ONLY to filter recipient contact seeding, so mailto-unsubscribe traffic
/// never becomes a "person I know" and pollutes triage. Combines local-part
/// prefixes, domain first-label hints, and token-like locals (hex/UUID blobs).
/// Ordinary addresses — including a dotted local like `rentbikes.net` — must pass.
pub fn is_robot_address(addr: &str) -> bool {
    let addr = addr.trim().to_ascii_lowercase();
    let (local, domain) = match addr.split_once('@') {
        Some((l, d)) if !l.is_empty() && !d.is_empty() => (l, d),
        // No parseable local@domain — not our concern here; let it through.
        _ => return false,
    };

    // Domain first-label hints (e.g. leave.mcmap.chase.com, unsub.beehiiv.com).
    let first_label = domain.split('.').next().unwrap_or("");
    const DOMAIN_ROBOT_LABELS: &[&str] = &[
        "unsub",
        "unsubscribe",
        "leave",
        "bounce",
        "optout",
        "opt-out",
    ];
    if DOMAIN_ROBOT_LABELS.contains(&first_label) {
        return true;
    }

    // Segment on the plus-address boundary so a prefix on ANY +-segment
    // (e.g. "unsubscribe-mc.us22_...") is caught.
    const LOCAL_ROBOT_PREFIXES: &[&str] = &[
        "unsubscribe",
        "unsub",
        "leave-",
        "optout",
        "opt-out",
        "bounce",
        "noreply",
        "no-reply",
        "donotreply",
        "do-not-reply",
        "list-",
    ];
    let plus_segments: Vec<&str> = local.split('+').collect();
    for seg in &plus_segments {
        for p in LOCAL_ROBOT_PREFIXES {
            if seg.starts_with(p) {
                return true;
            }
        }
        // "*.optout" style suffix on a segment (e.g. dxirq3pb.560xwm.9t9eb.optout).
        if seg.ends_with(".optout") || seg.ends_with("-optout") {
            return true;
        }
    }

    // Token-like locals: opaque machine blobs no human would choose. First,
    // multiple +-separated UUID-ish segments (beehiiv style).
    let uuidish_segments = plus_segments.iter().filter(|s| looks_uuidish(s)).count();
    if uuidish_segments >= 1 && plus_segments.len() >= 2 {
        return true;
    }
    for seg in &plus_segments {
        if looks_uuidish(seg) || is_hex_blob(seg) {
            return true;
        }
    }

    // Break the local on the usual separators and inspect each run, so a real
    // dotted name (rentbikes.net) never trips it — its runs are short and
    // vowel-rich.
    for run in local.split(['.', '-', '_', '=']) {
        if is_token_blob(run) {
            return true;
        }
    }

    false
}

/// A long random-looking machine token: >=25 alphanumeric chars, at least one
/// digit, vowel ratio below 20%. Human words carry far more vowels even when long.
fn is_token_blob(s: &str) -> bool {
    if s.len() < 25 || !s.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return false;
    }
    let has_digit = s.bytes().any(|b| b.is_ascii_digit());
    if !has_digit {
        return false;
    }
    let vowels = s.bytes().filter(|b| b"aeiou".contains(b)).count();
    vowels * 5 < s.len()
}

/// A UUID shape: 8-4-4-4-12 hex groups separated by hyphens.
fn looks_uuidish(s: &str) -> bool {
    let groups: Vec<&str> = s.split('-').collect();
    if groups.len() != 5 {
        return false;
    }
    let lens = [8, 4, 4, 4, 12];
    groups
        .iter()
        .zip(lens.iter())
        .all(|(g, &n)| g.len() == n && g.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// A long opaque hex/token blob: >=20 chars drawn only from `[0-9a-f-]`. Real
/// names contain letters outside a-f and stay under the threshold, so they pass.
fn is_hex_blob(s: &str) -> bool {
    if s.len() < 20 {
        return false;
    }
    s.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}

/// Capture unsubscribe intent: the raw trimmed `List-Unsubscribe` value, plus
/// whether RFC 8058 one-click is advertised (`List-Unsubscribe-Post` containing
/// `List-Unsubscribe=One-Click`). One-click is forced `false` without a
/// `List-Unsubscribe` header. Only raw material — parsing is the endpoint's job.
pub fn extract_unsub_headers(msg: &mail_parser::Message) -> (Option<String>, bool) {
    let list_unsub = msg
        .header_raw("List-Unsubscribe")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let one_click = list_unsub.is_some()
        && msg
            .header_raw("List-Unsubscribe-Post")
            .map(|v| {
                v.to_ascii_lowercase()
                    .contains("list-unsubscribe=one-click")
            })
            .unwrap_or(false);
    (list_unsub, one_click)
}

/// The ONLY authserv-id we trust. Gmail stamps its verdict with this id on
/// delivery; a header carrying any other id is one the sender could have typed.
const GMAIL_AUTHSERV_ID: &str = "mx.google.com";

/// Written by Google's own infrastructure on the inbound SMTP path, above the
/// verdict it stamps.
const GOOGLE_SOURCE_HEADER: &str = "X-Google-Smtp-Source";

/// Written ONLY on mail Gmail PULLED with POP ("Check mail from other
/// accounts") — a path that runs no inbound authentication whatsoever, so
/// whatever `Authentication-Results` the message carries is the sender's text.
const GMAIL_POP_HEADER: &str = "X-Gmail-Fetch-Info";

const AUTH_RESULTS_HEADER: &str = "Authentication-Results";

/// Read the email-authentication verdict from the TOPMOST `Authentication-Results`
/// header. `Some(true)` = DMARC passed and bound to the From domain, or DKIM
/// passed with a signing domain aligned to it. `Some(false)` = Gmail evaluated
/// it and neither held. `None` = nothing evaluable (no header, not Gmail's
/// authserv-id, no corroboration that Gmail handled the message, unparseable) —
/// never an assertion of pass.
///
/// TOPMOST, NOT LAST. Gmail PREPENDS its own `Authentication-Results` when it
/// accepts inbound mail and does NOT strip copies the sender wrote, so every
/// occurrence below the first is attacker-controlled text. `Message::header_raw`
/// resolves a name to its LAST occurrence — exactly the forgeable one. Nor can
/// this walk `headers_raw()`: that is a `filter_map` which DROPS any header
/// whose raw bytes are not valid UTF-8, and Gmail's own header is not
/// guaranteed decodable because it echoes the envelope sender verbatim — an
/// attacker who puts a high byte there would see their genuine verdict skipped
/// and a header they wrote lower down promoted to "topmost". So this walks
/// `headers()` (EVERY header, document order), takes the first one NAMED
/// `Authentication-Results`, and slices the raw bytes itself. An undecodable
/// genuine verdict is absence of evidence, never licence to look further down.
///
/// TWO `From` HEADERS => `None`. That is malformed per RFC 5322, and here it is
/// a live attack primitive: `Message::from` resolves to the LAST `From` while
/// the verdict is read off the FIRST `Authentication-Results`, so a sender who
/// authenticates genuinely as their own domain could staple a second `From`
/// below it and have the verdict attributed to a domain they do not own.
///
/// CORROBORATION. `mx.google.com` in the authserv-id is just a string the
/// sender can type; Gmail only runs inbound authentication on mail delivered to
/// its SMTP edge, NOT on mail pulled via POP or placed by
/// `users.messages.import`/`insert`. So a Google-written header must appear
/// ABOVE the verdict in document order (an `X-Google-Smtp-Source`, or a
/// `Received` whose `by` clause is `mx.google.com` — both are stamped by the
/// same hop that stamps the verdict, above it), and the POP marker must be
/// absent.
///
/// RESIDUAL GAP, precisely: this is header-shape evidence, not a signature.
/// Nothing in the bytes is cryptographically bound to Google, so on an
/// ingestion path where Gmail prepends nothing — POP with the marker somehow
/// absent, or `messages.import`/`insert` performed with the user's own
/// credentials — a sender who writes BOTH a fake `Received: ... by
/// mx.google.com` and a fake verdict still gets through. Closing that needs
/// out-of-band evidence this parser cannot see (the Gmail API's own delivery
/// metadata), and the blast radius is bounded: the verdict only ever gates the
/// tracking-pixel bypass for an already-known contact.
pub fn extract_auth_pass(msg: &mail_parser::Message, from_addr: &str) -> Option<bool> {
    let headers = msg.headers();
    if headers
        .iter()
        .filter(|h| matches!(h.name, mail_parser::HeaderName::From))
        .count()
        > 1
    {
        return None;
    }
    if headers
        .iter()
        .any(|h| h.name.as_str().eq_ignore_ascii_case(GMAIL_POP_HEADER))
    {
        return None;
    }
    let at = headers
        .iter()
        .position(|h| h.name.as_str().eq_ignore_ascii_case(AUTH_RESULTS_HEADER))?;
    if !headers[..at]
        .iter()
        .any(|h| google_wrote(h, &msg.raw_message))
    {
        return None;
    }
    let header = &headers[at];
    let bytes = msg
        .raw_message
        .get(header.offset_start as usize..header.offset_end as usize)?;
    auth_results_verdict(std::str::from_utf8(bytes).ok()?, from_addr)
}

/// Is this header one Google's inbound path wrote? Either the
/// `X-Google-Smtp-Source` stamp, or a `Received` handed off `by mx.google.com`.
/// The `by` clause is matched as a whole token so `by mx.google.com.evil.tld`
/// cannot satisfy it. A header whose raw bytes are not UTF-8 corroborates
/// nothing.
fn google_wrote(header: &mail_parser::Header, raw_message: &[u8]) -> bool {
    let name = header.name.as_str();
    if name.eq_ignore_ascii_case(GOOGLE_SOURCE_HEADER) {
        return true;
    }
    if !name.eq_ignore_ascii_case("Received") {
        return false;
    }
    let Some(value) = raw_message
        .get(header.offset_start as usize..header.offset_end as usize)
        .and_then(|b| std::str::from_utf8(b).ok())
    else {
        return false;
    };
    let tokens: Vec<&str> = value.split_whitespace().collect();
    tokens.windows(2).any(|w| {
        w[0].eq_ignore_ascii_case("by")
            && w[1]
                .trim_end_matches([';', '.', ','])
                .eq_ignore_ascii_case(GMAIL_AUTHSERV_ID)
    })
}

/// Evaluate one raw `Authentication-Results` value (RFC 8601) against the
/// RFC5322 From address. Pure, so the whole trust decision is testable on header
/// text alone.
///
/// EVERY `pass` MUST BIND TO THE FROM DOMAIN. `dmarc=pass` counts only when its
/// `header.from=` — the domain Gmail itself evaluated — aligns with the From
/// domain checked here; a `dmarc=pass` with a missing or mismatched
/// `header.from` is no evidence at all, because a sender who owns
/// `attacker.tld` genuinely passes DMARC for `attacker.tld`.
///
/// ALIGNMENT POLICY, deliberately narrow and stated here because the code cannot
/// show what was rejected: the domains must be EQUAL, or the From domain must be
/// a dot-suffixed child of the signing domain (`d=example.com` aligns with
/// `From: x@news.example.com`). The reverse — a signature from a SUBDOMAIN
/// standing in for the parent — is rejected: on a multi-tenant suffix, a tenant
/// who controls `evil.shared.example` could otherwise authenticate mail claiming
/// `From: billing@shared.example`. No public-suffix list is consulted (residual:
/// a signature by a registry suffix such as `co.uk` would align with
/// `victim.co.uk`, which needs a DKIM key for the suffix itself and so is not
/// attacker-reachable).
pub fn auth_results_verdict(raw: &str, from_addr: &str) -> Option<bool> {
    let from_domain = addr_domain(from_addr)?;
    let fields = tokenize_auth_results(raw)?;
    let mut fields = fields.iter();
    // authserv-id, optionally followed by an RFC 8601 version number.
    let authserv = fields.next()?.first()?;
    if !authserv.eq_ignore_ascii_case(GMAIL_AUTHSERV_ID) {
        return None;
    }

    let mut evaluated = false;
    let mut verdict = false;
    for field in fields {
        let Some((method, result)) = field[0].split_once('=') else {
            continue;
        };
        evaluated = true;
        if !result.eq_ignore_ascii_case("pass") {
            continue;
        }
        let props = || field[1..].iter().map(String::as_str);
        if method.eq_ignore_ascii_case("dmarc") {
            if let Some(evaluated_from) = dmarc_header_from(props())
                && domains_aligned(&evaluated_from, &from_domain)
            {
                verdict = true;
            }
            continue;
        }
        if method.eq_ignore_ascii_case("dkim")
            && let Some(signing) = dkim_signing_domain(props())
            && domains_aligned(&signing, &from_domain)
        {
            verdict = true;
        }
    }
    // A Gmail header that ran no method at all ("mx.google.com; none") is an
    // absence of evidence, not a failure.
    evaluated.then_some(verdict)
}

/// Split one `Authentication-Results` value into fields of tokens in ONE pass,
/// tracking RFC 5322 comment nesting AND quoted-string state together: `;` ends
/// a field and whitespace ends a token ONLY at comment depth 0 and outside
/// quotes. Both guards are load-bearing — Gmail's SPF comment routinely carries
/// `;` and `=`, and it echoes the envelope MAIL FROM verbatim into
/// `smtp.mailfrom=`, so a sender using `<"x; dmarc=pass y"@attacker.tld>` would
/// otherwise manufacture a field the caller reads as a real method result. A
/// quoted string stays inside the token that carries it (quotes and all), so its
/// contents can never become a token of their own; a comment is dropped but
/// still breaks the token, so it can never glue two together. `None` for a
/// malformed value — an unterminated comment or quoted string, where the rest of
/// the field list would be silently discarded.
fn tokenize_auth_results(raw: &str) -> Option<Vec<Vec<String>>> {
    let mut fields: Vec<Vec<String>> = Vec::new();
    let mut tokens: Vec<String> = Vec::new();
    let mut token = String::new();
    let mut depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    for c in raw.chars() {
        if escaped {
            escaped = false;
            if quoted && depth == 0 {
                token.push(c);
            }
            continue;
        }
        // A quoted string suspends BOTH scanners, comment included: Gmail echoes
        // the envelope MAIL FROM a second time inside the SPF comment
        // ("domain of <sender> designates …"), and an RFC 5321 quoted local-part
        // may legally contain `)`. Letting that `)` close the comment would put
        // the rest of the sender's own text back at depth 0, where a `;` starts
        // a field the caller reads as a real method result. Inside a comment the
        // characters are still discarded — only the delimiters are neutralized.
        if quoted {
            match c {
                '\\' => escaped = true,
                '"' => {
                    quoted = false;
                    if depth == 0 {
                        token.push('"');
                    }
                }
                _ => {
                    if depth == 0 {
                        token.push(c);
                    }
                }
            }
            continue;
        }
        if depth > 0 {
            match c {
                '\\' => escaped = true,
                '"' => quoted = true,
                '(' => depth += 1,
                ')' => depth -= 1,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => {
                quoted = true;
                token.push('"');
            }
            '(' | ')' | ';' | ' ' | '\t' | '\r' | '\n' => {
                if !token.is_empty() {
                    tokens.push(std::mem::take(&mut token));
                }
                match c {
                    '(' => depth += 1,
                    ';' => fields.push(std::mem::take(&mut tokens)),
                    _ => {}
                }
            }
            _ => token.push(c),
        }
    }
    if depth != 0 || quoted || escaped {
        return None;
    }
    if !token.is_empty() {
        tokens.push(token);
    }
    fields.push(tokens);
    fields.retain(|f| !f.is_empty());
    Some(fields)
}

/// The domain a `dmarc=` result was evaluated against, from its `header.from=`
/// property spec. Gmail writes the bare domain; an `addr-spec` form is tolerated
/// by taking the part after the last `@`.
fn dmarc_header_from<'a>(tokens: impl Iterator<Item = &'a str>) -> Option<String> {
    for token in tokens {
        let Some((key, val)) = token.split_once('=') else {
            continue;
        };
        if !key.eq_ignore_ascii_case("header.from") {
            continue;
        }
        let val = val.trim_matches('"');
        let val = val.rsplit_once('@').map_or(val, |(_, d)| d);
        return (!val.is_empty()).then(|| val.to_string());
    }
    None
}

/// The signing domain of one `dkim=` result, from its property specs.
/// `header.d` is authoritative. When only the AUID `header.i=` is present (the
/// form Gmail usually emits) it is accepted, which DELEGATES the binding to
/// Gmail's enforcement of RFC 6376 §3.5: a verifier must reject a signature
/// whose `i=` domain is neither `d=` nor a subdomain of it, so the signer cannot
/// name a domain it does not sign for. When BOTH are present that relationship
/// is checked here rather than assumed, and a violation yields no signing domain
/// at all.
fn dkim_signing_domain<'a>(tokens: impl Iterator<Item = &'a str>) -> Option<String> {
    let mut d: Option<String> = None;
    let mut i: Option<String> = None;
    for token in tokens {
        let Some((key, val)) = token.split_once('=') else {
            continue;
        };
        let val = val.trim_matches('"');
        if key.eq_ignore_ascii_case("header.d") && d.is_none() {
            d = (!val.is_empty()).then(|| val.to_ascii_lowercase());
        }
        if key.eq_ignore_ascii_case("header.i") && i.is_none() {
            i = val
                .rsplit_once('@')
                .map(|(_, dom)| dom)
                .filter(|dom| !dom.is_empty())
                .map(str::to_ascii_lowercase);
        }
    }
    match (d, i) {
        (Some(d), Some(i)) => (i == d || i.ends_with(&format!(".{d}"))).then_some(d),
        (Some(d), None) => Some(d),
        (None, i) => i,
    }
}

/// The domain half of an addr-spec, lower-cased. `None` for an address with no
/// `@` or an empty domain — which makes the whole verdict `None`.
fn addr_domain(addr: &str) -> Option<String> {
    let (_, domain) = addr.trim().rsplit_once('@')?;
    let domain = domain.trim().trim_end_matches('.').to_ascii_lowercase();
    (!domain.is_empty()).then_some(domain)
}

/// Alignment per the policy documented on [`auth_results_verdict`]: equal, or
/// the From domain is a dot-suffixed child of `signing`. One direction only.
fn domains_aligned(signing: &str, from_domain: &str) -> bool {
    let signing = signing.trim().trim_end_matches('.').to_ascii_lowercase();
    let from_domain = from_domain
        .trim()
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if signing.is_empty() || from_domain.is_empty() {
        return false;
    }
    signing == from_domain
        || from_domain
            .strip_suffix(&signing)
            .is_some_and(|parent| parent.ends_with('.') && parent.len() > 1)
}

/// Derive a stable thread key from headers when the Gmail thread id is absent:
/// the root References id, else In-Reply-To, else this message's own Message-ID.
pub fn fallback_thread_id(msg: &mail_parser::Message) -> Option<String> {
    // The first References id is the thread root.
    if let Some(first) = msg
        .references()
        .as_text_list()
        .and_then(|l| l.iter().next())
    {
        return Some(first.to_string());
    }
    if let Some(irt) = msg
        .in_reply_to()
        .as_text_list()
        .and_then(|l| l.iter().next())
    {
        return Some(irt.to_string());
    }
    msg.message_id().map(|s| s.to_string())
}

/// A tiny FNV-1a hash, hex-encoded: the last-resort stable id when even a
/// Message-ID is missing, so two such messages don't collide on "".
pub fn stable_hash(input: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in input.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// INGEST CAPS. Bytes are stored only up to these limits; a part over the
/// per-attachment cap, or one that would push the message's stored total over the
/// per-message cap, keeps its metadata but drops its bytes (`data: None`).
const ATTACHMENT_CAP_BYTES: usize = 10 * 1024 * 1024;
const MESSAGE_ATTACHMENT_TOTAL_CAP_BYTES: usize = 25 * 1024 * 1024;

/// Extract every attachment part from a parsed message — REAL attachments
/// (`Content-Disposition: attachment`) AND cid-inline parts
/// (`Content-Disposition: inline` with a `Content-ID`, how templates embed
/// logos). mail-parser's `attachments()` iterator already yields exactly this set
/// (everything that is not a displayed text/html body part).
///
/// Caps are applied here (see the consts above): the metadata of an over-cap
/// part is always kept (`size_bytes` is its real decoded size), but its bytes are
/// dropped (`data: None`) so the store lands a NULL blob. Filename falls back to
/// `attachment-<n>` (1-based part order) when the part declares none; the mime is
/// the part's declared type, falling back to `application/octet-stream`.
///
/// This runs for BOTH sealed and non-sealed mail — storage is fine either way;
/// the byte-serving endpoint is what guards sealed parents.
pub fn extract_attachments(m: &mail_parser::Message) -> Vec<AttachmentInfo> {
    let mut out = Vec::new();
    let mut stored_total: usize = 0;
    for (i, part) in m.attachments().enumerate() {
        // Multipart containers carry no bytes of their own; mail-parser should
        // not list them as attachments, but skip defensively.
        if part.is_multipart() {
            continue;
        }
        let bytes = part.contents();
        let size = bytes.len();
        let filename = part
            .attachment_name()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("attachment-{}", i + 1));
        let mime = part
            .content_type()
            .map(|ct| match ct.subtype() {
                Some(sub) => format!("{}/{}", ct.ctype(), sub),
                None => ct.ctype().to_string(),
            })
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "application/octet-stream".to_string());
        // Keep bytes only when this part is within the per-attachment cap AND
        // keeping it would not push the message's stored total over its cap.
        let within_per = size <= ATTACHMENT_CAP_BYTES;
        let within_total = stored_total.saturating_add(size) <= MESSAGE_ATTACHMENT_TOTAL_CAP_BYTES;
        let data = if within_per && within_total {
            stored_total += size;
            Some(bytes.to_vec())
        } else {
            None
        };
        out.push(AttachmentInfo {
            filename,
            mime,
            size_bytes: size as i64,
            data,
        });
    }
    out
}

/// Turn raw fetched bytes into a fully-triaged, store-ready message.
///
/// `known_contact_lookup` is called ONLY for non-sealed mail, with the parsed
/// from-address, so the caller can consult the contacts table. Sealed mail
/// short-circuits before this is ever invoked.
pub fn ingest(
    fetched: &RawFetched,
    cfg: &Stage1Config,
    now: DateTime<Utc>,
    mut known_contact_lookup: impl FnMut(&str) -> bool,
) -> TriagedMessage {
    let parsed = MessageParser::default().parse(&fetched.raw);

    // Recipients (To + Cc) — only meaningful for Sent mail, where they become
    // BOTH the contacts seed and the stored display recipients. Collected here
    // while the parse is in hand. Received mail collects nothing, so `to_addrs`
    // stays NULL on it.
    let mut mailboxes: Vec<(String, Option<String>)> = Vec::new();
    if fetched.is_sent
        && let Some(m) = &parsed
    {
        if let Some(to) = m.to() {
            collect_mailboxes(to, &mut mailboxes);
        }
        if let Some(cc) = m.cc() {
            collect_mailboxes(cc, &mut mailboxes);
        }
    }
    let to_addrs = format_recipients(&mailboxes);
    let mut recipients: Vec<String> = mailboxes.into_iter().map(|(addr, _)| addr).collect();

    // Extract fields with graceful fallbacks for malformed mail.
    #[allow(clippy::type_complexity)]
    let (
        from_addr,
        from_name,
        subject,
        received_at,
        thread_id,
        msg_id_hdr,
        text,
        body_html,
        list_unsubscribe,
        list_unsub_one_click,
        auth_pass,
    ) = match &parsed {
        Some(m) => {
            let (fa, fname) = m.from().map(first_addr).unwrap_or_default();
            let subject = m.subject().unwrap_or("").to_string();
            let received = m
                .date()
                .and_then(|d| DateTime::parse_from_rfc3339(&d.to_rfc3339()).ok())
                .map(|d| d.with_timezone(&Utc))
                .or(fetched.internal_date)
                .unwrap_or(now);
            // Prefer a plain-text body; fall back to HTML flattened to text.
            // This flattened `text` is the UNCHANGED path that feeds triage,
            // FTS, and the agent door — it must not be affected by the HTML
            // work below.
            //
            // IMPORTANT: for an alternative carrying ONLY an HTML part,
            // mail-parser registers that part under `text_body` too, so
            // `body_text(0)` would run MAIL-PARSER'S html-to-text — which
            // glues adjacent block elements ("EPIC SteakTable for 4") and
            // leaks `<script type=...>` content (JSON-LD blobs), wrecking
            // every word-boundary regex and the LLM/FTS/embed text
            // downstream. Check the actual part type and route real HTML
            // through OUR flattener, exactly like the body_html capture
            // below.
            let text = match m.text_part(0) {
                Some(p) if !p.is_text_html() => p.text_contents().unwrap_or_default().to_string(),
                _ => match m.html_part(0).and_then(|p| p.text_contents()) {
                    Some(h) => html_to_text(h),
                    None => String::new(),
                },
            };
            // Separately: capture the RENDERED HTML body (when present) and
            // sanitize it server-side for the human door. `None` for
            // plain-text-only mail (leaves body_html NULL). This never feeds
            // triage/FTS/MCP — only GET /client/thread/{id}.
            //
            // IMPORTANT: mail-parser's `body_html(0)` SYNTHESIZES HTML from a
            // text/plain part when no real HTML alternative exists (it wraps
            // the text in <p> tags), and `html_body_count()` counts that
            // synthetic entry. We must NOT store that — a genuinely
            // plain-text-only email has to leave body_html NULL. So we check
            // the actual part type (`is_text_html`) and only capture a REAL
            // `text/html` MIME part.
            let body_html = m
                .html_part(0)
                .filter(|p| p.is_text_html())
                .and_then(|_| m.body_html(0))
                .map(|c| sanitize_email_html(&c))
                .filter(|s| !s.trim().is_empty());
            let thr = fetched
                .gmail_thread_id
                .clone()
                .or_else(|| fallback_thread_id(m));
            let (list_unsub, one_click) = extract_unsub_headers(m);
            let auth_pass = extract_auth_pass(m, &fa);
            (
                fa,
                fname,
                subject,
                received,
                thr,
                m.message_id().map(|s| s.to_string()),
                text,
                body_html,
                list_unsub,
                one_click,
                auth_pass,
            )
        }
        None => (
            String::new(),
            None,
            String::new(),
            fetched.internal_date.unwrap_or(now),
            fetched.gmail_thread_id.clone(),
            None,
            String::new(),
            None,
            None,
            false,
            None,
        ),
    };

    // A gmail_msg_id is required to key the row. Fall back to Message-ID, then a
    // hash of the raw bytes so nothing collides on an empty string.
    let gmail_msg_id = if !fetched.gmail_msg_id.is_empty() {
        fetched.gmail_msg_id.clone()
    } else if let Some(mid) = &msg_id_hdr {
        stable_hash(mid)
    } else {
        stable_hash(&String::from_utf8_lossy(&fetched.raw))
    };

    let thread_id = thread_id.unwrap_or_else(|| gmail_msg_id.clone());

    // Attachments (real + cid-inline), capped. Extracted once here and moved into
    // whichever TriagedMessage return path fires. Present for sealed mail too —
    // storage is fine; the byte-serving endpoint guards sealed parents.
    let attachments = parsed.as_ref().map(extract_attachments).unwrap_or_default();

    // A compact snippet for list views; body text drives triage.
    let snippet: String = text.chars().take(200).collect();

    // Finalize the contact recipients (Sent mail only): drop the account's OWN
    // address and the From address (on Sent mail From == the account), case-fold
    // and dedup. This is the explicit guard that the user's own address can
    // never become a contact.
    let self_addr = fetched.account_addr.trim().to_ascii_lowercase();
    let from_lc = from_addr.trim().to_ascii_lowercase();
    let mut seen: Vec<String> = Vec::new();
    recipients.retain(|r| {
        let lc = r.trim().to_ascii_lowercase();
        if lc.is_empty() || lc == self_addr || lc == from_lc || seen.contains(&lc) {
            return false;
        }
        // Drop machine/robot recipients (Gmail mailto-unsubscribe traffic goes to
        // real addresses that must never become "people I know" contacts).
        if is_robot_address(&lc) {
            return false;
        }
        seen.push(lc);
        true
    });

    let message = NewMessage {
        account_id: fetched.account_id,
        gmail_msg_id,
        thread_id,
        from_addr: from_addr.clone(),
        from_name,
        subject: subject.clone(),
        received_at,
        snippet,
        body: text.clone(),
        body_html,
        is_sent: fetched.is_sent,
        to_addrs,
        list_unsubscribe,
        list_unsub_one_click,
        auth_pass,
    };

    // ---- SEAL DETECTION FIRST (security invariant) ----------------------
    let seal_kind = seal::detect_sealed(&SealInput {
        from_addr: &from_addr,
        subject: &subject,
        body: &text,
    });
    if let Some(kind) = seal_kind {
        // Sealed: importance 0, no Stage-1, no deadline, never confident enough
        // to matter — it will never be surfaced or sent to an LLM.
        return TriagedMessage {
            message,
            recipients,
            sensitivity: Sensitivity::Sealed,
            sealed_kind: Some(kind),
            importance: 0,
            tier: Tier::Noise,
            one_line: String::new(),
            reason: format!("sealed at ingest ({})", kind.as_str()),
            field_reasons: FieldReasons::default(),
            matched_rule: None,
            deadline: None,
            shipment: None,
            ship_extract: false,
            receipt: None,
            calendar: None,
            attachments,
            confident: true,
        };
    }

    // ---- Sent mail: seed contacts, but DO NOT run Stage-1 triage ----------
    // The user's own outbox must never pollute the ranked inbox. We write a
    // neutral tier=noise/importance=0 row (belt: ranked_updates/search also
    // exclude is_sent=1) and skip the LLM path entirely. Recipients still seed
    // the contacts table via `ingest_message`, and ride along on the row itself
    // as `to_addrs` for the human door's sent listing.
    if fetched.is_sent {
        return TriagedMessage {
            message,
            recipients,
            sensitivity: Sensitivity::Normal,
            sealed_kind: None,
            importance: 0,
            tier: Tier::Noise,
            one_line: String::new(),
            reason: "sent mail (contacts seeded; not triaged)".to_string(),
            field_reasons: FieldReasons::default(),
            matched_rule: None,
            deadline: None,
            shipment: None,
            ship_extract: false,
            receipt: None,
            calendar: None,
            attachments,
            confident: true,
        };
    }

    // ---- Non-sealed: derive known-contact, load rules already provided --
    let is_known = known_contact_lookup(&from_addr);
    // Sender rules are matched inside stage1; the caller supplies them via cfg's
    // sibling argument. We accept them through the wrapper below.
    let result = stage1_with_config(&message, is_known, &[], cfg, now);

    // SHIPMENT DETECTION runs INDEPENDENTLY of the triage tier: a "your order
    // shipped" email is noise-tier for the ranked inbox but still feeds the
    // package tracker. Only ever runs here, on the NON-SEALED path — a sealed OTP
    // short-circuited above and never reaches this line.
    let shipment = shipment::detect_shipment(&from_addr, &subject, &text);

    // SHIPMENTS-EXTRACTOR TRIGGER, next to detection and on the SAME non-sealed,
    // non-sent path: the LOOSE signal (no tracking number required) that stamps
    // `triage.ship_extract_model='pending'` so the shipments specialist picks the
    // row up. Wider than `detect_shipment` on purpose — an order confirmation
    // with no number is exactly the mail the regex cannot handle and the model
    // can. Sealed mail short-circuited above, so this never sees sealed content.
    let ship_extract = shipment::has_loose_shipping_signal(&from_addr, &subject, &text);

    // RECEIPT DETECTION runs INDEPENDENTLY of the triage tier AND of shipment
    // detection: a receipt (record of money already paid) is noise-tier for the
    // ranked inbox but feeds the Receipts category, and an order-confirmation with
    // a total AND tracking is BOTH a receipt and a shipment. Only ever runs here,
    // on the NON-SEALED path — a sealed OTP short-circuited above and never reaches
    // this line, so a receipt can never carry sealed data. When a receipt is
    // present, the store's ingest write force-resolves this message's triage row
    // (status='done') so it never surfaces as inbox clutter.
    let receipt = receipt::detect_receipt(&from_addr, &subject, &text);

    // CALENDAR DETECTION runs INDEPENDENTLY of the triage tier, exactly like
    // receipts: an invite/cancellation/RSVP is a record of scheduling state
    // that feeds the Calendar category (and is auto-resolved to 'done' by the
    // store's ingest write). Only ever runs here, on the NON-SEALED path — a
    // sealed OTP short-circuited above and never reaches this line, so a
    // calendar update can never carry sealed data.
    let calendar = calendar::detect_calendar(
        &from_addr,
        message.from_name.as_deref(),
        &subject,
        &text,
        received_at,
    );

    TriagedMessage {
        message,
        recipients,
        sensitivity: Sensitivity::Normal,
        sealed_kind: None,
        importance: result.importance,
        tier: result.tier,
        one_line: result.one_line,
        reason: result.reason,
        field_reasons: result.field_reasons,
        matched_rule: result.matched_rule,
        deadline: result.deadline,
        shipment,
        ship_extract,
        receipt,
        calendar,
        attachments,
        confident: result.confident,
    }
}

/// Full ingest with sender rules. Kept separate so the common test path
/// ([`ingest`]) needs no rules argument, while the sync engine passes the
/// account's rule list.
pub fn ingest_with_rules(
    fetched: &RawFetched,
    cfg: &Stage1Config,
    now: DateTime<Utc>,
    rules: &[crate::types::SenderRule],
    known_contact_lookup: impl FnMut(&str) -> bool,
) -> TriagedMessage {
    // Reuse the seal-first path from `ingest`, then, if it came back normal,
    // re-run Stage-1 WITH rules. This keeps the seal invariant in exactly one
    // place while still honoring user rules.
    let mut triaged = ingest(fetched, cfg, now, known_contact_lookup);
    // Sealed and Sent mail never run Stage-1 (Sent is neutral tier-noise), so
    // they must not run the rules re-pass either.
    if triaged.sensitivity == Sensitivity::Sealed || fetched.is_sent || rules.is_empty() {
        return triaged;
    }
    let is_known = triaged.matched_rule.is_none() && triaged.reason.contains("known contact");
    let result = stage1_with_config(&triaged.message, is_known, rules, cfg, now);
    triaged.importance = result.importance;
    triaged.tier = result.tier;
    triaged.one_line = result.one_line;
    triaged.reason = result.reason;
    triaged.field_reasons = result.field_reasons;
    triaged.matched_rule = result.matched_rule;
    triaged.deadline = result.deadline;
    triaged.confident = result.confident;
    triaged
}

/// Ingest ONE message the account just sent, from its raw RFC822 bytes.
///
/// Pure bytes-in: the caller (squelch-api's send path, which alone holds write
/// credentials) does the Gmail fetch and hands the decoded bytes over, so core
/// stays Gmail-WRITE-free. Runs the same seal-first pipeline as the sync engine
/// with `is_sent: true`, so no LLM is ever called and the row lands neutral
/// (tier=noise, importance=0); the attention/search queries filter `is_sent=0`,
/// so an echoed message creates no attention noise. Idempotent: the store's
/// `UNIQUE(account_id, gmail_msg_id)` upsert makes a re-ingest of the same Gmail
/// id a no-op update, returning the existing local id.
///
/// A SEALED OUTBOUND COPY IS NOT WRITTEN: `Ok(None)`, nothing committed. Seal
/// detection runs BEFORE the `is_sent` branch (that ordering is the security
/// invariant), so a reply quoting an OTP trips it. Committing that row would put a
/// sealed message in the thread, and `thread_guard_and_subject` 404s any thread
/// holding one, so echoing the reply would HIDE the counterparty's mail the user
/// was reading a second ago. Skipping degrades to "your reply appears on the next
/// backfill", which is exactly the pre-echo status quo.
#[allow(clippy::too_many_arguments)] // the parts of one Gmail fetch, nothing more
pub fn ingest_sent(
    store: &SqliteStore,
    account_id: AccountId,
    account_addr: &str,
    gmail_msg_id: &str,
    gmail_thread_id: Option<String>,
    raw: Vec<u8>,
    internal_date: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> crate::error::Result<Option<i64>> {
    let fetched = RawFetched {
        account_id,
        gmail_msg_id: gmail_msg_id.to_string(),
        gmail_thread_id,
        raw,
        internal_date,
        is_sent: true,
        account_addr: account_addr.to_string(),
    };
    // Both arguments are unreachable on the sent path: Stage-1 (which cfg tunes)
    // and the contact lookup only run for non-sealed RECEIVED mail.
    let triaged = ingest(&fetched, &Stage1Config::default(), now, |_| false);
    if triaged.sensitivity == Sensitivity::Sealed {
        return Ok(None);
    }
    store.ingest_message(&triaged).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(account_id: AccountId, msgid: &str, bytes: &str, is_sent: bool) -> RawFetched {
        RawFetched {
            account_id,
            gmail_msg_id: msgid.to_string(),
            gmail_thread_id: Some(format!("thr-{msgid}")),
            raw: bytes.as_bytes().to_vec(),
            internal_date: Some(Utc::now()),
            is_sent,
            account_addr: "me@example.com".to_string(),
        }
    }

    #[test]
    fn html_flatten_strips_tags_and_entities() {
        let t = html_to_text("<p>Hello&nbsp;<b>world</b> &amp; <i>friends</i></p>");
        assert_eq!(t, "Hello world & friends");
    }

    #[test]
    fn entity_decode_covers_numeric_named_and_zero_width() {
        // Preheader padding (&zwnj;) must vanish entirely, not survive as
        // literal "&zwnj;" tokens in the text fed to FTS/LLM/embeddings.
        let t =
            html_to_text("<p>Hi&zwnj;&zwnj; there &#8212; it&#x2019;s 6&nbsp;pm &copy;2026</p>");
        assert_eq!(t, "Hi there \u{2014} it\u{2019}s 6 pm \u{A9}2026");
        // Unknown entities and bare ampersands pass through literally.
        let t = html_to_text("<p>AT&T &customentity; a &amp; b</p>");
        assert_eq!(t, "AT&T &customentity; a & b");
        // Raw zero-width characters (already decoded upstream) are removed too.
        let t = html_to_text("<p>a\u{200C}b\u{200B}c</p>");
        assert_eq!(t, "abc");
    }

    #[test]
    fn sealed_otp_lands_sealed_with_importance_zero() {
        let eml = "From: Bank <noreply@bank.com>\r\n\
                   To: me@example.com\r\n\
                   Subject: Your verification code\r\n\
                   Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   Your one-time passcode is 483920. Enter this code to continue.\r\n";
        let f = raw(1, "g-otp", eml, false);
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |_| false);
        assert_eq!(t.sensitivity, Sensitivity::Sealed);
        assert!(t.sealed_kind.is_some());
        assert_eq!(t.importance, 0);
        assert!(t.deadline.is_none());
    }

    #[test]
    fn dated_bill_lands_deadline_tier() {
        let eml = "From: Acme <invoices@acme.com>\r\n\
                   To: me@example.com\r\n\
                   Subject: Invoice #4402 from Acme\r\n\
                   Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   Your invoice total is $1,299.00. Payment due by August 15, 2026.\r\n";
        let f = raw(1, "g-bill", eml, false);
        let now = DateTime::parse_from_rfc3339("2026-07-07T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let t = ingest(&f, &Stage1Config::default(), now, |_| false);
        assert_eq!(t.sensitivity, Sensitivity::Normal);
        assert_eq!(t.tier, Tier::Deadline);
        let d = t.deadline.expect("deadline extracted");
        assert_eq!(d.amount, Some(1299.00));
        assert!(!d.past_due);
    }

    #[test]
    fn ebay_return_refund_is_not_a_past_due_bill() {
        // The real inbox case (today = 2026-07-09): an eBay RETURN REFUND arriving
        // "by July 13th" was mis-triaged as a past-due bill "104 weeks after due
        // date". It must now produce NO bill tier, NO past_due, and no deadline row.
        let eml = "From: eBay <ebay@ebay.com>\r\n\
                   To: me@example.com\r\n\
                   Subject: Your return refund is on its way\r\n\
                   Date: Wed, 9 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   Your eBay return refund of $23.99 will be issued by July 13th.\r\n";
        let f = raw(1, "g-ebay", eml, false);
        let now = DateTime::parse_from_rfc3339("2026-07-09T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let t = ingest(&f, &Stage1Config::default(), now, |_| false);
        assert_ne!(t.tier, Tier::PastDue, "refund must not be past-due");
        assert_ne!(t.tier, Tier::Deadline, "refund must not be a deadline");
        assert!(t.deadline.is_none(), "no deadline row for a refund");
    }

    #[test]
    fn yearless_genuine_bill_resolves_to_receipt_year_future() {
        // A genuine bill "due July 13" (no year) received 2026-07-09 resolves to
        // 2026-07-13 (this year, future) => Deadline tier, NOT past_due.
        let eml = "From: Acme <billing@acme.com>\r\n\
                   To: me@example.com\r\n\
                   Subject: Invoice #900\r\n\
                   Date: Wed, 9 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   Amount due $50.00. Payment due July 13th.\r\n";
        let f = raw(1, "g-yearless", eml, false);
        let now = DateTime::parse_from_rfc3339("2026-07-09T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let t = ingest(&f, &Stage1Config::default(), now, |_| true);
        assert_eq!(t.tier, Tier::Deadline);
        let d = t.deadline.expect("deadline extracted");
        assert!(!d.past_due, "future year-less date is not past-due");
        assert_eq!(
            d.due_at,
            DateTime::parse_from_rfc3339("2026-07-13T23:59:59Z")
                .unwrap()
                .with_timezone(&Utc)
        );
    }

    #[test]
    fn yearless_recently_passed_bill_is_past_due_by_days() {
        // "due July 1" received 2026-07-09 => 2026-07-01 (past by days, within the
        // 14-day grace) => PastDue, legitimately, not "weeks/years" late.
        let eml = "From: Acme <billing@acme.com>\r\n\
                   To: me@example.com\r\n\
                   Subject: Invoice #901\r\n\
                   Date: Wed, 9 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   Amount due $50.00. Payment due July 1.\r\n";
        let f = raw(1, "g-recent", eml, false);
        let now = DateTime::parse_from_rfc3339("2026-07-09T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let t = ingest(&f, &Stage1Config::default(), now, |_| true);
        let d = t.deadline.expect("deadline extracted");
        assert!(d.past_due);
        // Due date is 2026-07-01, ~8 days before receipt — days, not weeks.
        let days = (now - d.due_at).num_days();
        assert!((7..=9).contains(&days), "past by days, got {days}");
    }

    #[test]
    fn html_only_body_is_flattened_before_triage() {
        let eml = "From: News <news@substack.com>\r\n\
                   Subject: The Weekly Roundup\r\n\
                   Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
                   Content-Type: text/html; charset=utf-8\r\n\
                   \r\n\
                   <html><body><p>Great stuff. <a href=\"x\">Unsubscribe</a> | Manage preferences</p></body></html>\r\n";
        let f = raw(1, "g-news", eml, false);
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |_| false);
        assert_eq!(t.sensitivity, Sensitivity::Normal);
        assert_eq!(t.tier, Tier::Noise);
        assert!(t.message.body.contains("Unsubscribe"));
        assert!(!t.message.body.contains('<'));
    }

    #[test]
    fn html_only_alternative_uses_our_flattener_not_mail_parsers() {
        // THE OPENTABLE BUG: an alternative with ONLY an HTML part lands in
        // mail-parser's `text_body` list, and its converter glues block
        // elements ("SteakTable for 4") and leaks <script> JSON-LD — which
        // broke every word-boundary regex (the reservation detector saw
        // "BoyntonConfirmation #"). Ours must separate blocks, drop scripts,
        // and the reservation must then detect end-to-end.
        let eml = "From: OpenTable <reservations@opentable.com>\r\n\
                   To: me@example.com\r\n\
                   Subject: Your reservation confirmation for EPIC Steak\r\n\
                   Date: Fri, 31 Jul 2026 19:47:00 +0000\r\n\
                   MIME-Version: 1.0\r\n\
                   Content-Type: multipart/alternative; boundary=X\r\n\
                   \r\n\
                   --X\r\n\
                   Content-Type: text/html; charset=utf-8\r\n\
                   \r\n\
                   <div>EPIC Steak</div><div>Table for 4 on Monday, August 3, 2026 at 6:00 pm</div>\
                   <div>Name: Braelyn Boynton</div><div>Confirmation #: 2110425567</div>\
                   <script type=\"application/ld+json\">{\"@type\":\"FoodEstablishmentReservation\"}</script>\r\n\
                   --X--\r\n";
        let f = raw(1, "g-resv", eml, false);
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |_| false);

        assert!(
            t.message.body.contains("Boynton Confirmation #"),
            "blocks must be separated: {}",
            t.message.body
        );
        assert!(
            t.message.body.contains("Steak Table for 4"),
            "blocks must be separated: {}",
            t.message.body
        );
        assert!(
            !t.message.body.contains("FoodEstablishmentReservation"),
            "script content must not leak: {}",
            t.message.body
        );

        let c = t.calendar.expect("reservation detected end-to-end");
        assert_eq!(c.kind, crate::triage::CalendarKind::Reservation);
        assert_eq!(c.event_title.as_deref(), Some("EPIC Steak"));
    }

    #[test]
    fn known_contact_lookup_is_consulted_for_normal_mail() {
        let eml = "From: Alice <alice@friends.com>\r\n\
                   Subject: dinner plans\r\n\
                   Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   friday at 7?\r\n";
        let f = raw(1, "g-alice", eml, false);
        let mut asked = Vec::new();
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |addr| {
            asked.push(addr.to_string());
            true
        });
        assert_eq!(t.tier, Tier::Signal);
        assert!(asked.iter().any(|a| a == "alice@friends.com"));
    }

    #[test]
    fn sent_mail_derives_contacts_from_recipients_not_self() {
        // From is the account (self); To/Cc are the real contacts.
        let eml = "From: Me <me@example.com>\r\n\
                   To: Alice <alice@friends.com>\r\n\
                   Cc: bob@friends.com\r\n\
                   Subject: dinner\r\n\
                   Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   see you friday\r\n";
        let f = raw(1, "g-sent", eml, /* is_sent */ true);
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |_| false);
        // recipients hold alice + bob, never self.
        assert!(t.recipients.iter().any(|r| r == "alice@friends.com"));
        assert!(t.recipients.iter().any(|r| r == "bob@friends.com"));
        assert!(!t.recipients.iter().any(|r| r == "me@example.com"));
        // Sent mail is not triaged: neutral noise / importance 0.
        assert_eq!(t.tier, Tier::Noise);
        assert_eq!(t.importance, 0);
    }

    #[test]
    fn sent_mail_stores_display_recipients_received_mail_stores_none() {
        // The display string is To then Cc in header order, `Name <addr>` where a
        // name exists and bare otherwise — and it is NOT filtered the way contact
        // seeding is: `support@` is who this went to, whatever the contacts table
        // thinks of it.
        let eml = "From: Me <me@example.com>\r\n\
                   To: Alice <alice@friends.com>, bob@friends.com\r\n\
                   Cc: unsubscribe@unsub.spmta.com\r\n\
                   Subject: dinner\r\n\
                   Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   see you friday\r\n";
        let f = raw(1, "g-to", eml, /* is_sent */ true);
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |_| false);
        assert_eq!(
            t.message.to_addrs.as_deref(),
            Some("Alice <alice@friends.com>, bob@friends.com, unsubscribe@unsub.spmta.com")
        );
        // The contacts seed keeps its own, narrower rules.
        assert!(!t.recipients.iter().any(|r| r.contains("unsub")));

        // Received mail has no "to" worth showing: the column stays NULL.
        let f = raw(1, "g-recv", eml, /* is_sent */ false);
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |_| false);
        assert_eq!(t.message.to_addrs, None);
    }

    #[test]
    fn recipient_names_holding_commas_are_requoted_for_the_client_splitter() {
        // The client splits the stored list on unquoted commas, so an
        // address-book lastname-first name must come back out quoted exactly as
        // the header carried it — unquoted, "Doe, John" would read as two
        // recipients ("Doe" and "John <j@x.com>").
        let eml = "From: Me <me@example.com>\r\n\
                   To: \"Doe, John\" <john@friends.com>, Plain Name <p@friends.com>\r\n\
                   Subject: dinner\r\n\
                   Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   see you friday\r\n";
        let f = raw(1, "g-quoted", eml, /* is_sent */ true);
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |_| false);
        assert_eq!(
            t.message.to_addrs.as_deref(),
            Some("\"Doe, John\" <john@friends.com>, Plain Name <p@friends.com>")
        );
    }

    #[test]
    fn sent_mail_never_seeds_self_even_when_to_is_self() {
        let eml = "From: me@example.com\r\n\
                   To: me@example.com\r\n\
                   Subject: note to self\r\n\
                   Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   reminder\r\n";
        let f = raw(1, "g-self", eml, true);
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |_| false);
        assert!(
            t.recipients.is_empty(),
            "self address must never be a contact"
        );
    }

    #[test]
    fn received_mail_seeds_no_contacts() {
        let eml = "From: Alice <alice@friends.com>\r\n\
                   To: me@example.com\r\n\
                   Subject: hi\r\n\
                   Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   hello\r\n";
        let f = raw(1, "g-recv", eml, /* is_sent */ false);
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |_| false);
        assert!(t.recipients.is_empty());
    }

    #[test]
    fn robot_addresses_are_filtered() {
        // Every one of these is a live example that MUST be filtered.
        let robots = [
            "leave-HXZRUFGHTN2UJLNONA7FQQ27HY.110064@leave.mcmap.chase.com",
            "9b284cf8-cebd-451c-95f9-cb939dc4682d+dac84f53-1111-2222-3333-444455556666+xyz@unsub.beehiiv.com",
            "unsubscribe-mc.us22_89497e127e8f1447718905808.aef79637c2-4c8c73a87a@unsubscribe.mailchimpapp.net",
            "dxirq3pb.560xwm.9t9eb.optout@e2ma.net",
            "unsubscribe@gf.d.sender-sib.com",
            "unsubscribe@unsub.spmta.com",
            "d6f58aa9b599316889f7d3cc20bf13bc@hous.craigslist.org",
            "1axcsnai4asp830zv6mplv6pvulamp169hk3nf-bboynton97=gmail.com@bf02.na2.hubspotemail.net",
            "097a2550-566e-11e6-83f0-002590e879ee@unsub.r.groupon.com",
        ];
        for r in robots {
            assert!(is_robot_address(r), "should be filtered as robot: {r}");
        }
    }

    #[test]
    fn real_people_survive() {
        // Real people that MUST pass through as contacts.
        let people = [
            "ellie@elliehuxtable.com",
            "bam@bamteamre.com",
            "cameron@tcpre.com",
            "rentbikes.net@gmail.com",
        ];
        for p in people {
            assert!(!is_robot_address(p), "should NOT be filtered: {p}");
        }
    }

    #[test]
    fn sent_mail_drops_robot_recipients_keeps_people() {
        let eml = "From: Me <me@example.com>\r\n\
                   To: Alice <alice@friends.com>, unsubscribe@unsub.spmta.com\r\n\
                   Cc: d6f58aa9b599316889f7d3cc20bf13bc@hous.craigslist.org\r\n\
                   Subject: mixed\r\n\
                   Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   body\r\n";
        let f = raw(1, "g-mixed", eml, true);
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |_| false);
        assert!(t.recipients.iter().any(|r| r == "alice@friends.com"));
        assert!(!t.recipients.iter().any(|r| r.contains("unsub")));
        assert!(!t.recipients.iter().any(|r| r.contains("craigslist")));
    }

    #[test]
    fn shipping_email_produces_a_shipment() {
        let eml = "From: UPS <ship-confirm@ups.com>\r\n\
                   To: me@example.com\r\n\
                   Subject: Your order of Wireless Headphones has shipped\r\n\
                   Date: Wed, 9 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   Your UPS package is on its way. Tracking number 1Z999AA10123456784.\r\n";
        let f = raw(1, "g-ship", eml, false);
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |_| false);
        let s = t.shipment.expect("shipment detected");
        assert_eq!(s.carrier, "ups");
        assert_eq!(s.tracking_number, "1Z999AA10123456784");
        assert_eq!(s.item_name, "Wireless Headphones");
        // Shipping mail is noise-tier for the ranked inbox.
        assert_eq!(t.tier, Tier::Noise);
    }

    #[test]
    fn bay_wheels_receipt_produces_a_receipt_row_with_amount() {
        // The live bug: a Bay Wheels ride receipt landed in Newsletters. It must
        // now classify as a receipt (with its total) and drop from inbox clutter.
        let eml = "From: Bay Wheels <no-reply@baywheels.com>\r\n\
                   To: me@example.com\r\n\
                   Subject: Your Bay Wheels ride receipt\r\n\
                   Date: Wed, 9 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   Thanks for riding! Receipt for your ride. Total: $3.49.\r\n";
        let f = raw(1, "g-baywheels", eml, false);
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |_| false);
        let r = t.receipt.expect("receipt detected");
        assert_eq!(r.amount, Some(3.49));
        // Receipts are noise-tier for the ranked inbox.
        assert_eq!(t.tier, Tier::Noise);
    }

    #[test]
    fn order_confirmation_receipt_extracts_total() {
        let eml = "From: Shop <orders@shop.com>\r\n\
                   To: me@example.com\r\n\
                   Subject: Order confirmation #12345\r\n\
                   Date: Wed, 9 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   Thank you for your order. Order total $3.49.\r\n";
        let f = raw(1, "g-order", eml, false);
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |_| false);
        assert_eq!(t.receipt.expect("receipt").amount, Some(3.49));
    }

    #[test]
    fn refund_is_not_a_receipt_at_ingest() {
        let eml = "From: eBay <ebay@ebay.com>\r\n\
                   To: me@example.com\r\n\
                   Subject: Your refund receipt\r\n\
                   Date: Wed, 9 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   Your refund of $23.99 has been issued to your card.\r\n";
        let f = raw(1, "g-refund", eml, false);
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |_| false);
        assert!(t.receipt.is_none(), "a refund must not be a receipt");
    }

    #[test]
    fn sealed_otp_never_produces_a_receipt() {
        // A sealed OTP short-circuits before receipt detection ever runs.
        let eml = "From: Bank <noreply@bank.com>\r\n\
                   To: me@example.com\r\n\
                   Subject: Your verification code\r\n\
                   Date: Wed, 9 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   Your one-time passcode is 483920. Thank you for your payment.\r\n";
        let f = raw(1, "g-otp-rcpt", eml, false);
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |_| false);
        assert_eq!(t.sensitivity, Sensitivity::Sealed);
        assert!(
            t.receipt.is_none(),
            "sealed mail must never yield a receipt"
        );
    }

    #[test]
    fn google_invite_produces_a_calendar_update_at_ingest() {
        // A Google Calendar invite: classified as a calendar update (with title
        // + start extracted) so the ingest write auto-resolves it out of the
        // attention bands and into the Calendar category.
        let eml = "From: Sam Doe <sam@gmail.com>\r\n\
                   To: me@example.com\r\n\
                   Subject: Invitation: Design review @ Wed Jul 22, 2026 10am - 11am (PDT) (me@example.com)\r\n\
                   Date: Mon, 20 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   Sam Doe has invited you. View on Google Calendar.\r\n";
        let f = raw(1, "g-cal-inv", eml, false);
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |_| false);
        let c = t.calendar.expect("calendar update detected");
        assert_eq!(c.kind, crate::triage::CalendarKind::Invite);
        assert_eq!(c.event_title.as_deref(), Some("Design review"));
        assert!(c.starts_at.is_some());
    }

    #[test]
    fn newsletter_mentioning_calendar_is_not_a_calendar_update_at_ingest() {
        let eml = "From: Shop News <news@shop.com>\r\n\
                   To: me@example.com\r\n\
                   Subject: Your July events calendar is here!\r\n\
                   Date: Mon, 20 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   Check our calendar of sales events. Unsubscribe here.\r\n";
        let f = raw(1, "g-cal-news", eml, false);
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |_| false);
        assert!(
            t.calendar.is_none(),
            "topical calendar prose must not classify"
        );
    }

    #[test]
    fn sealed_otp_never_produces_a_calendar_update() {
        // A sealed OTP short-circuits before calendar detection ever runs, even
        // with an invitation-shaped subject.
        let eml = "From: Bank <noreply@bank.com>\r\n\
                   To: me@example.com\r\n\
                   Subject: Your verification code\r\n\
                   Date: Mon, 20 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   Your one-time passcode is 483920. Invitation: security review @ Wed Jul 22, 2026 10am.\r\n";
        let f = raw(1, "g-otp-cal", eml, false);
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |_| false);
        assert_eq!(t.sensitivity, Sensitivity::Sealed);
        assert!(
            t.calendar.is_none(),
            "sealed mail must never yield a calendar update"
        );
    }

    #[test]
    fn sealed_otp_never_produces_a_shipment() {
        // A sealed OTP short-circuits before shipment detection ever runs.
        let eml = "From: Bank <noreply@bank.com>\r\n\
                   To: me@example.com\r\n\
                   Subject: Your verification code\r\n\
                   Date: Wed, 9 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   Your one-time passcode is 483920123456. Enter this code to continue.\r\n";
        let f = raw(1, "g-otp2", eml, false);
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |_| false);
        assert_eq!(t.sensitivity, Sensitivity::Sealed);
        assert!(
            t.shipment.is_none(),
            "sealed mail must never yield a shipment"
        );
    }

    #[test]
    fn list_unsubscribe_headers_land_on_the_message() {
        // A newsletter advertising both a mailto and an https one-click endpoint,
        // with RFC 8058 List-Unsubscribe-Post. Both fields must be captured.
        let eml = "From: News <news@substack.com>\r\n\
                   To: me@example.com\r\n\
                   Subject: The Weekly Roundup\r\n\
                   Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
                   List-Unsubscribe: <mailto:unsub@substack.com?subject=bye>, <https://substack.com/u/9?x=1>\r\n\
                   List-Unsubscribe-Post: List-Unsubscribe=One-Click\r\n\
                   \r\n\
                   Great stuff this week.\r\n";
        let f = raw(1, "g-unsub", eml, false);
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |_| false);
        let lu = t
            .message
            .list_unsubscribe
            .expect("list-unsubscribe captured");
        assert!(lu.contains("mailto:unsub@substack.com"));
        assert!(lu.contains("https://substack.com/u/9"));
        assert!(
            t.message.list_unsub_one_click,
            "RFC 8058 one-click detected"
        );
    }

    #[test]
    fn list_unsubscribe_without_post_is_not_one_click() {
        let eml = "From: News <news@substack.com>\r\n\
                   To: me@example.com\r\n\
                   Subject: Roundup\r\n\
                   Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
                   List-Unsubscribe: <https://substack.com/u/9>\r\n\
                   \r\n\
                   body\r\n";
        let f = raw(1, "g-unsub2", eml, false);
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |_| false);
        assert!(t.message.list_unsubscribe.is_some());
        assert!(
            !t.message.list_unsub_one_click,
            "no List-Unsubscribe-Post => not one-click"
        );
    }

    #[test]
    fn no_unsub_header_leaves_fields_empty() {
        let eml = "From: Alice <alice@friends.com>\r\n\
                   Subject: dinner\r\n\
                   Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   friday?\r\n";
        let f = raw(1, "g-plain-unsub", eml, false);
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |_| false);
        assert!(t.message.list_unsubscribe.is_none());
        assert!(!t.message.list_unsub_one_click);
    }

    // ---- email authentication (Authentication-Results) -------------------

    /// A real-shaped Gmail verdict line, comments and all.
    fn gmail_ar(body: &str) -> String {
        format!("mx.google.com;\r\n       {body}")
    }

    #[test]
    fn no_auth_results_header_is_unknown_not_a_pass() {
        assert_eq!(auth_results_verdict("", "a@example.com"), None);
        let eml = "From: Alice <alice@friends.com>\r\n\
                   Subject: dinner\r\n\
                   Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   friday?\r\n";
        let f = raw(1, "g-no-ar", eml, false);
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |_| false);
        assert_eq!(t.message.auth_pass, None);
    }

    #[test]
    fn dmarc_pass_is_a_pass() {
        let ar = gmail_ar(
            "dkim=pass header.i=@github.com header.s=pf2023 header.b=Qk9nZTE7;\r\n       \
             spf=pass (google.com: domain of noreply@github.com designates 140.82.113.1 \
             as permitted sender) smtp.mailfrom=noreply@github.com;\r\n       \
             dmarc=pass (p=REJECT sp=REJECT dis=NONE) header.from=github.com",
        );
        assert_eq!(
            auth_results_verdict(&ar, "noreply@github.com"),
            Some(true),
            "dmarc=pass is the primary path"
        );
        // …and stands on its own even when DKIM is not aligned.
        let ar = gmail_ar("dkim=pass header.i=@sendgrid.net; dmarc=pass header.from=shop.com");
        assert_eq!(auth_results_verdict(&ar, "hi@shop.com"), Some(true));
    }

    #[test]
    fn dkim_pass_needs_the_signing_domain_aligned_to_from() {
        // Aligned exactly.
        let ar = gmail_ar("dkim=pass header.i=@stripe.com header.s=s1; spf=pass");
        assert_eq!(auth_results_verdict(&ar, "receipts@stripe.com"), Some(true));
        // Aligned as a parent domain (From is a subdomain of d=).
        let ar = gmail_ar("dkim=pass header.d=stripe.com header.s=s1");
        assert_eq!(
            auth_results_verdict(&ar, "receipts@mail.stripe.com"),
            Some(true)
        );
        // MISALIGNED: a genuine signature from somebody else's domain is not a
        // statement about this From header — the whole spoofing case.
        let ar = gmail_ar("dkim=pass header.i=@evil-mailer.net header.s=s1; spf=pass");
        assert_eq!(auth_results_verdict(&ar, "boss@stripe.com"), Some(false));
        // A suffix that is not a LABEL boundary must not align.
        let ar = gmail_ar("dkim=pass header.d=stripe.com");
        assert_eq!(auth_results_verdict(&ar, "x@notstripe.com"), Some(false));
        // header.d wins over header.i when both are present.
        let ar = gmail_ar("dkim=pass header.i=@stripe.com header.d=evil.net");
        assert_eq!(auth_results_verdict(&ar, "x@stripe.com"), Some(false));
    }

    #[test]
    fn dkim_and_dmarc_failing_is_an_explicit_fail() {
        let ar = gmail_ar(
            "dkim=fail header.i=@paypal.com header.s=s1;\r\n       \
             spf=softfail (google.com: domain of transitioning x@paypal.com does not \
             designate 1.2.3.4 as permitted sender) smtp.mailfrom=x@paypal.com;\r\n       \
             dmarc=fail (p=NONE sp=NONE dis=NONE) header.from=paypal.com",
        );
        assert_eq!(auth_results_verdict(&ar, "service@paypal.com"), Some(false));
    }

    #[test]
    fn a_foreign_authserv_id_is_unknown_because_the_sender_could_have_written_it() {
        let ar = "evil.example.com; dkim=pass header.i=@bank.com; dmarc=pass header.from=bank.com";
        assert_eq!(auth_results_verdict(ar, "alerts@bank.com"), None);
        // Gmail's id with an RFC 8601 version number still counts.
        let ar = "mx.google.com 1; dmarc=pass header.from=bank.com";
        assert_eq!(auth_results_verdict(ar, "alerts@bank.com"), Some(true));
        // A header that names no method ran nothing: unknown, not a failure.
        assert_eq!(auth_results_verdict("mx.google.com; none", "a@b.com"), None);
        // No From domain to align against => nothing to decide.
        assert_eq!(
            auth_results_verdict("mx.google.com; dmarc=pass", "alice"),
            None
        );
    }

    #[test]
    fn only_the_topmost_header_counts_so_a_forged_copy_below_cannot_win() {
        // Gmail PREPENDS its own verdict and leaves the sender's copy in place.
        // The forged one sits below and must be ignored entirely — note that
        // `header_raw` would have returned exactly this one.
        let eml = "Delivered-To: me@example.com\r\n\
                   X-Google-Smtp-Source: AGHT+IF7q9Zx\r\n\
                   Received: from mail-ed1-f41.google.com (mail-ed1-f41.google.com. \
                   [209.85.208.41])\r\n\tby mx.google.com with SMTPS id abc123 for \
                   <me@example.com>;\r\n\tMon, 07 Jul 2026 03:00:00 -0700 (PDT)\r\n\
                   Authentication-Results: mx.google.com;\r\n\
                   \tdkim=fail header.i=@friend.com header.s=s1;\r\n\
                   \tspf=fail (google.com: domain of x does not designate 1.2.3.4 as \
                   permitted sender) smtp.mailfrom=x@attacker.tld;\r\n\
                   \tdmarc=fail (p=NONE) header.from=friend.com\r\n\
                   From: Friend <pal@friend.com>\r\n\
                   Subject: hey\r\n\
                   Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
                   Authentication-Results: mx.google.com; dkim=pass header.i=@friend.com; \
                   dmarc=pass header.from=friend.com\r\n\
                   \r\n\
                   check this out\r\n";
        let f = raw(1, "g-forged", eml, false);
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |_| false);
        assert_eq!(
            t.message.auth_pass,
            Some(false),
            "the genuine topmost verdict wins over the forged copy below it"
        );

        // The mirror image: the genuine header passes, a forged failing copy
        // below cannot revoke it.
        let eml = eml
            .replace("dkim=fail header.i=@friend.com header.s=s1", "dkim=pass header.i=@friend.com header.s=s1")
            .replace("dmarc=fail (p=NONE) header.from=friend.com", "dmarc=pass (p=NONE) header.from=friend.com")
            .replace(
                "Authentication-Results: mx.google.com; dkim=pass header.i=@friend.com; dmarc=pass header.from=friend.com",
                "Authentication-Results: mx.google.com; dkim=fail; dmarc=fail",
            );
        let f = raw(1, "g-forged2", &eml, false);
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |_| false);
        assert_eq!(t.message.auth_pass, Some(true));
    }

    #[test]
    fn folding_and_comment_whitespace_do_not_change_the_verdict() {
        // Folded across four lines with tabs, and an SPF comment carrying both
        // ';' and '=' — which would shred a naive split on ';'.
        let folded = "mx.google.com;\r\n\tdkim=pass\r\n\t header.i=@notify.example.com\r\n\
                      \t header.s=20230601 header.b=AbC/dEf+;\r\n\
                      \tspf=pass (google.com: domain of bounce+7=x@notify.example.com \
                      designates 209.85.220.41 as permitted sender; relaxed) \
                      smtp.mailfrom=\"bounce+7=x@notify.example.com\";\r\n\
                      \tdmarc=fail (p=NONE sp=NONE dis=NONE) header.from=notify.example.com";
        assert_eq!(
            auth_results_verdict(folded, "hello@notify.example.com"),
            Some(true),
            "aligned dkim=pass survives folding and a ';'-bearing comment"
        );
        // A comment must never glue two tokens into one.
        let ar = "mx.google.com; dkim=pass(ok)header.d=example.com";
        assert_eq!(auth_results_verdict(ar, "x@example.com"), Some(true));
        // An escaped paren inside a comment does not end it early.
        let ar = "mx.google.com; dkim=pass (a \\) dmarc=pass) header.d=evil.net";
        assert_eq!(auth_results_verdict(ar, "x@example.com"), Some(false));
    }

    /// The headers Google writes above its own verdict on real inbound mail.
    const GOOGLE_PREAMBLE: &str = "Delivered-To: me@example.com\r\n\
         X-Google-Smtp-Source: AGHT+IF7q9Zx\r\n\
         Received: from mail-ed1-f41.google.com (mail-ed1-f41.google.com. \
         [209.85.208.41])\r\n\tby mx.google.com with SMTPS id abc123 for \
         <me@example.com>;\r\n\tMon, 07 Jul 2026 03:00:00 -0700 (PDT)\r\n";

    /// Run the verdict exactly as [`ingest`] does — From address taken from the
    /// same parse — so header-level attacks are exercised end to end.
    fn auth_pass_of(bytes: &[u8]) -> Option<bool> {
        let parsed = MessageParser::default().parse(bytes).expect("parses");
        let (from, _) = parsed.from().map(first_addr).unwrap_or_default();
        extract_auth_pass(&parsed, &from)
    }

    /// Gmail echoes the envelope MAIL FROM verbatim into
    /// `smtp.mailfrom=`, so a sender using
    /// `MAIL FROM:<"x; dmarc=pass y"@attacker.tld>` puts a semicolon inside a
    /// quoted string in Gmail's own GENUINE, CORRECT verdict. A splitter with no
    /// quoted-string state reads the tail as a field and manufactures a
    /// `dmarc=pass` nobody wrote.
    #[test]
    fn a_semicolon_inside_the_quoted_envelope_sender_cannot_manufacture_a_field() {
        let ar = "mx.google.com;\r\n \
                  dkim=fail header.i=@friend.com header.s=s1;\r\n \
                  spf=pass (google.com: domain of \"x; dmarc=pass y\"@attacker.tld \
                  designates 1.2.3.4 as permitted sender) \
                  smtp.mailfrom=\"x; dmarc=pass y\"@attacker.tld;\r\n \
                  dmarc=fail (p=NONE sp=NONE dis=NONE) header.from=friend.com";
        assert_eq!(
            auth_results_verdict(ar, "pal@friend.com"),
            Some(false),
            "a quoted ';' is data, not a field separator"
        );
        // The same guard the other way round: an open paren inside a quoted
        // string is a literal and must not start a comment that swallows the
        // rest of the value.
        let ar = "mx.google.com; spf=pass smtp.mailfrom=\"a(b\"@attacker.tld; \
                  dkim=pass header.d=friend.com";
        assert_eq!(auth_results_verdict(ar, "pal@friend.com"), Some(true));
    }

    /// The same echo, one layer deeper. Gmail writes the envelope sender TWICE:
    /// once in `smtp.mailfrom=` and once inside the SPF comment ("domain of
    /// <sender> designates …"). A quoted local-part may legally carry `)`, so a
    /// comment scanner that ignores quotes lets the sender close the comment
    /// early and drop the rest of their own text back at depth 0, where a `;`
    /// starts a field. The attacker owns attacker.tld, so SPF genuinely passes
    /// and Gmail's verdict is entirely correct — the forgery is in the parse.
    #[test]
    fn a_quoted_paren_inside_the_spf_comment_cannot_close_it() {
        let payloads = [
            "dmarc=pass header.from=bank.com",
            "dkim=pass header.d=bank.com",
            "dkim=pass header.i=@bank.com",
        ];
        for payload in payloads {
            let sender = format!("\"x); {payload}; y(\"@attacker.tld");
            let ar = format!(
                "mx.google.com;\r\n \
                 spf=pass (google.com: domain of {sender} designates 203.0.113.9 \
                 as permitted sender) smtp.mailfrom={sender};\r\n \
                 dmarc=fail (p=REJECT sp=REJECT dis=NONE) header.from=bank.com"
            );
            assert_ne!(
                auth_results_verdict(&ar, "billing@bank.com"),
                Some(true),
                "comment-escape forged a pass for {payload}"
            );
        }
        // Gmail's own escaping of a structural character stays readable, and a
        // genuine verdict for the same domain still passes.
        let ar = "mx.google.com;\r\n \
                  spf=pass (google.com: domain of bob@bank.com designates \
                  203.0.113.9 as permitted sender) smtp.mailfrom=bob@bank.com;\r\n \
                  dmarc=pass (p=REJECT) header.from=bank.com";
        assert_eq!(auth_results_verdict(ar, "billing@bank.com"), Some(true));
    }

    /// `dmarc=pass` is a statement about the domain in its own
    /// `header.from=`. An attacker who owns `attacker.tld` passes DMARC there
    /// GENUINELY; unbound, that verdict would be read as a pass for whatever
    /// From header the message carries.
    #[test]
    fn dmarc_pass_must_be_bound_to_the_from_domain() {
        let ar = "mx.google.com; dkim=pass header.i=@attacker.tld header.s=s1; \
                  spf=pass smtp.mailfrom=bob@attacker.tld; \
                  dmarc=pass (p=NONE) header.from=attacker.tld";
        assert_eq!(
            auth_results_verdict(ar, "pal@friend.com"),
            Some(false),
            "a genuine pass for the attacker's own domain says nothing about From"
        );
        assert_eq!(auth_results_verdict(ar, "bob@attacker.tld"), Some(true));
        // A dmarc=pass carrying no header.from at all is no evidence.
        let ar = "mx.google.com; dmarc=pass (p=NONE)";
        assert_eq!(auth_results_verdict(ar, "pal@friend.com"), Some(false));
    }

    /// `Message::from` resolves to the LAST `From` while the verdict
    /// comes from the FIRST `Authentication-Results`, so two From headers let a
    /// sender who authenticates as their own domain have that verdict credited
    /// to a domain they do not own.
    #[test]
    fn two_from_headers_make_the_verdict_unknown() {
        let eml = format!(
            "{GOOGLE_PREAMBLE}\
             Authentication-Results: mx.google.com; dkim=pass header.i=@attacker.tld \
             header.s=s1; spf=pass smtp.mailfrom=bob@attacker.tld; dmarc=pass (p=NONE) \
             header.from=attacker.tld\r\n\
             From: Evil <evil@attacker.tld>\r\n\
             From: Pal <pal@friend.com>\r\n\
             Subject: hey\r\n\
             Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
             \r\n\
             check this out\r\n"
        );
        assert_eq!(auth_pass_of(eml.as_bytes()), None);
        let f = raw(1, "g-two-from", &eml, false);
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |_| false);
        assert_eq!(t.message.auth_pass, None);
        // One From, same verdict: still no pass, because it is not bound to it.
        let single = eml.replace("From: Evil <evil@attacker.tld>\r\n", "");
        assert_eq!(auth_pass_of(single.as_bytes()), Some(false));
    }

    /// `headers_raw()` is a `filter_map` that DROPS undecodable headers, so an
    /// attacker who gets a high byte echoed into Gmail's own verdict (via the
    /// `smtp.mailfrom=` echo) would see it skipped and their own copy below
    /// promoted to "topmost".
    #[test]
    fn an_undecodable_topmost_verdict_is_unknown_not_a_look_further_down() {
        let mut eml: Vec<u8> = GOOGLE_PREAMBLE.as_bytes().to_vec();
        eml.extend_from_slice(
            b"Authentication-Results: mx.google.com; dkim=fail header.i=@friend.com; \
              spf=pass smtp.mailfrom=",
        );
        eml.push(0xff);
        eml.extend_from_slice(
            b"@attacker.tld; dmarc=fail (p=NONE) header.from=friend.com\r\n\
              Authentication-Results: mx.google.com; dkim=pass header.i=@friend.com; \
              dmarc=pass header.from=friend.com\r\n\
              From: Pal <pal@friend.com>\r\n\
              Subject: hey\r\n\
              Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
              \r\n\
              check this out\r\n",
        );
        assert_eq!(
            auth_pass_of(&eml),
            None,
            "an unreadable genuine verdict is absence of evidence"
        );
    }

    /// `mx.google.com` as an authserv-id is just a string the sender
    /// typed unless Google's own headers corroborate that Google handled the
    /// message. Gmail runs no inbound authentication on POP-pulled mail or on
    /// `users.messages.import`/`insert`.
    #[test]
    fn a_verdict_with_no_google_written_header_above_it_is_unknown() {
        let ar = "Authentication-Results: mx.google.com; dkim=pass header.i=@friend.com; \
                  dmarc=pass header.from=friend.com\r\n";
        let tail = "From: Pal <pal@friend.com>\r\n\
                    Subject: hey\r\n\
                    Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
                    \r\n\
                    check this out\r\n";
        // Imported/inserted: nothing above the verdict but the sender's own text.
        let eml = format!("Delivered-To: me@example.com\r\n{ar}{tail}");
        assert_eq!(auth_pass_of(eml.as_bytes()), None);
        // A Received that was NOT handed off by mx.google.com corroborates
        // nothing, and the `by` clause is matched as a whole token.
        let eml =
            format!("Received: from x (x) by mx.google.com.evil.tld with SMTP id 1\r\n{ar}{tail}");
        assert_eq!(auth_pass_of(eml.as_bytes()), None);
        // Either genuine Google header, above the verdict, corroborates it.
        let eml = format!("X-Google-Smtp-Source: AGHT+IF7q9Zx\r\n{ar}{tail}");
        assert_eq!(auth_pass_of(eml.as_bytes()), Some(true));
        let eml = format!(
            "Received: from mail-ed1-f41.google.com (mail-ed1-f41.google.com. \
             [209.85.208.41]) by mx.google.com with SMTPS id abc123\r\n{ar}{tail}"
        );
        assert_eq!(auth_pass_of(eml.as_bytes()), Some(true));
        // Below the verdict it corroborates nothing — the sender wrote both.
        let eml = format!("{ar}X-Google-Smtp-Source: AGHT+IF7q9Zx\r\n{tail}");
        assert_eq!(auth_pass_of(eml.as_bytes()), None);
        // POP-pulled mail ran no inbound authentication at all.
        let eml = format!(
            "X-Gmail-Fetch-Info: me@other.com 5 pop.other.com 995 me\r\n{GOOGLE_PREAMBLE}{ar}{tail}"
        );
        assert_eq!(auth_pass_of(eml.as_bytes()), None);
    }

    /// Alignment consults no public-suffix list, so it accepts only the
    /// parent->child direction: a signature from a SUBDOMAIN must not stand in
    /// for its parent, which on a multi-tenant suffix is the reachable attack.
    #[test]
    fn alignment_accepts_only_the_parent_direction() {
        let ar = gmail_ar("dkim=pass header.d=evil-tenant.shared.example");
        assert_eq!(
            auth_results_verdict(ar.as_str(), "billing@shared.example"),
            Some(false),
            "a tenant subdomain cannot authenticate its parent"
        );
        // The probes that must stay failures for pal@friend.com.
        for signing in [
            "evil-gmail.com",
            "gmail.com.evil.com",
            "notfriend.com",
            "friend.com.attacker.tld",
        ] {
            let ar = gmail_ar(&format!("dkim=pass header.d={signing}"));
            assert_eq!(
                auth_results_verdict(&ar, "pal@friend.com"),
                Some(false),
                "must not align: {signing}"
            );
        }
        // The kept direction: the From domain is a child of the signing domain.
        let ar = gmail_ar("dkim=pass header.d=friend.com");
        assert_eq!(auth_results_verdict(&ar, "pal@mail.friend.com"), Some(true));
    }

    /// `header.i` is the AUID the SIGNER chooses; accepting it alone
    /// delegates to Gmail's RFC 6376 enforcement. When `header.d` is also
    /// present that relationship is checked here instead of assumed.
    #[test]
    fn header_i_is_only_trusted_when_it_agrees_with_header_d() {
        // i under d: fine, and d is what is used.
        let ar = gmail_ar("dkim=pass header.i=@mail.friend.com header.d=friend.com");
        assert_eq!(auth_results_verdict(&ar, "pal@friend.com"), Some(true));
        // i outside d: the pair is incoherent, so neither is usable.
        let ar = gmail_ar("dkim=pass header.i=@friend.com header.d=attacker.tld");
        assert_eq!(auth_results_verdict(&ar, "pal@friend.com"), Some(false));
        // i alone stays accepted (the form Gmail usually emits).
        let ar = gmail_ar("dkim=pass header.i=@friend.com header.s=s1");
        assert_eq!(auth_results_verdict(&ar, "pal@friend.com"), Some(true));
    }

    /// An unbalanced open paren or quote makes the whole value malformed. It
    /// must not run to end-of-value, silently discarding every field after it.
    #[test]
    fn an_unterminated_comment_or_quote_is_malformed_not_truncated() {
        let ar = "mx.google.com; dkim=fail (unclosed; dmarc=fail header.from=friend.com";
        assert_eq!(auth_results_verdict(ar, "pal@friend.com"), None);
        let ar = "mx.google.com; spf=pass smtp.mailfrom=\"unclosed@attacker.tld; \
                  dmarc=fail header.from=friend.com";
        assert_eq!(auth_results_verdict(ar, "pal@friend.com"), None);
    }

    /// Every confirmed bypass, in one place: NONE of them may ever produce
    /// `Some(true)`, which is the only value that opens the tracking-pixel gate.
    mod adversarial {
        use super::*;

        #[test]
        fn no_confirmed_bypass_produces_a_pass() {
            let victim = "pal@friend.com";
            let headers = [
                // A quoted ';' in the echoed envelope sender.
                "mx.google.com;\r\n dkim=fail header.i=@friend.com header.s=s1;\r\n \
                 spf=pass (google.com: domain of \"x; dmarc=pass y\"@attacker.tld \
                 designates 1.2.3.4 as permitted sender) \
                 smtp.mailfrom=\"x; dmarc=pass y\"@attacker.tld;\r\n \
                 dmarc=fail (p=NONE sp=NONE dis=NONE) header.from=friend.com",
                // A genuine pass for a domain the attacker owns.
                "mx.google.com; dkim=pass header.i=@attacker.tld header.s=s1; \
                 spf=pass smtp.mailfrom=bob@attacker.tld; \
                 dmarc=pass (p=NONE) header.from=attacker.tld",
                // A subdomain signature standing in for its parent.
                "mx.google.com; dkim=pass header.d=evil.friend.com.attacker.tld",
                // An AUID naming a domain the signature does not cover.
                "mx.google.com; dkim=pass header.i=@friend.com header.d=attacker.tld",
                // An unbalanced paren hiding the fields that follow.
                "mx.google.com; dkim=fail (unclosed; dmarc=pass header.from=friend.com",
            ];
            for ar in headers {
                assert_ne!(
                    auth_results_verdict(ar, victim),
                    Some(true),
                    "bypass produced a pass: {ar}"
                );
            }

            let tail = "From: Pal <pal@friend.com>\r\n\
                        Subject: hey\r\n\
                        Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
                        \r\n\
                        check this out\r\n";
            // Two From headers over a genuine attacker-domain verdict.
            let ar = "Authentication-Results: mx.google.com; dkim=pass \
                      header.i=@attacker.tld header.s=s1; spf=pass \
                      smtp.mailfrom=bob@attacker.tld; dmarc=pass (p=NONE) \
                      header.from=attacker.tld\r\n";
            let two_from = format!("{GOOGLE_PREAMBLE}{ar}From: Evil <evil@attacker.tld>\r\n{tail}");
            // A forged verdict with no Google header above it.
            let uncorroborated = format!(
                "Delivered-To: me@example.com\r\n\
                 Authentication-Results: mx.google.com; dkim=pass header.i=@friend.com; \
                 dmarc=pass header.from=friend.com\r\n{tail}"
            );
            for eml in [two_from, uncorroborated] {
                assert_ne!(
                    auth_pass_of(eml.as_bytes()),
                    Some(true),
                    "bypass produced a pass: {eml}"
                );
            }

            // An undecodable genuine verdict must not promote the forged copy
            // below it.
            let mut eml: Vec<u8> = GOOGLE_PREAMBLE.as_bytes().to_vec();
            eml.extend_from_slice(
                b"Authentication-Results: mx.google.com; dkim=fail header.i=@friend.com; \
                  spf=pass smtp.mailfrom=",
            );
            eml.push(0xff);
            eml.extend_from_slice(
                b"@attacker.tld; dmarc=fail (p=NONE) header.from=friend.com\r\n\
                  Authentication-Results: mx.google.com; dkim=pass header.i=@friend.com; \
                  dmarc=pass header.from=friend.com\r\n",
            );
            eml.extend_from_slice(tail.as_bytes());
            assert_ne!(auth_pass_of(&eml), Some(true));
        }
    }

    #[test]
    fn cjk_charsets_survive_ingest() {
        // QQ mail (and most Chinese/Japanese senders) encode headers as GBK/
        // GB2312 encoded-words and bodies as raw GBK. The decoders for those
        // charsets live behind mail-parser's `full_encoding` feature — WITHOUT
        // it every such header and body ingests as U+FFFD per character, which
        // the client renders as "??". This test is the tripwire for that
        // feature being dropped from the workspace dependency.
        //
        // The base64 runs are GBK bytes: xOO6ww== is 你好, xOO6w8rAvec= is
        // 你好世界.
        let eml = "From: =?gb2312?B?xOO6ww==?= <821715717@qq.com>\r\n\
                   To: me@example.com\r\n\
                   Subject: =?gb2312?B?xOO6w8rAvec=?=\r\n\
                   Date: Thu, 17 Jul 2026 06:31:00 +0000\r\n\
                   MIME-Version: 1.0\r\n\
                   Content-Type: text/plain; charset=gb2312\r\n\
                   Content-Transfer-Encoding: base64\r\n\
                   \r\n\
                   xOO6w8rAvec=\r\n";
        let f = raw(1, "g-gbk", eml, false);
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |_| false);
        assert_eq!(t.message.from_name.as_deref(), Some("你好"));
        assert_eq!(t.message.subject, "你好世界");
        assert!(
            t.message.body.contains("你好世界"),
            "gb2312 body must decode, got: {:?}",
            t.message.body
        );
        assert!(
            !t.message.from_name.as_deref().unwrap_or("").contains('\u{fffd}'),
            "a replacement character means the charset decoder is gone"
        );
    }

    #[test]
    fn attachments_extracted_with_caps_and_cid_inline() {
        // A real multipart/mixed with: a text body (NOT an attachment), a pdf, a
        // png, a cid-inline png (no filename), and an oversized octet-stream part
        // that must exceed the 10 MB per-attachment cap.
        let big = "A".repeat(11 * 1024 * 1024);
        let eml = format!(
            "From: Sender <s@example.com>\r\n\
             To: me@example.com\r\n\
             Subject: Files attached\r\n\
             Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
             MIME-Version: 1.0\r\n\
             Content-Type: multipart/mixed; boundary=\"BOUND\"\r\n\
             \r\n\
             --BOUND\r\n\
             Content-Type: text/plain; charset=utf-8\r\n\
             \r\n\
             Here are the files.\r\n\
             --BOUND\r\n\
             Content-Type: application/pdf; name=\"doc.pdf\"\r\n\
             Content-Disposition: attachment; filename=\"doc.pdf\"\r\n\
             Content-Transfer-Encoding: base64\r\n\
             \r\n\
             SGVsbG8=\r\n\
             --BOUND\r\n\
             Content-Type: image/png; name=\"pic.png\"\r\n\
             Content-Disposition: attachment; filename=\"pic.png\"\r\n\
             Content-Transfer-Encoding: base64\r\n\
             \r\n\
             d29ybGQ=\r\n\
             --BOUND\r\n\
             Content-Type: image/png\r\n\
             Content-ID: <logo@squelch>\r\n\
             Content-Disposition: inline\r\n\
             Content-Transfer-Encoding: base64\r\n\
             \r\n\
             aW5saW5l\r\n\
             --BOUND\r\n\
             Content-Type: application/octet-stream; name=\"big.bin\"\r\n\
             Content-Disposition: attachment; filename=\"big.bin\"\r\n\
             \r\n\
             {big}\r\n\
             --BOUND--\r\n"
        );
        let f = raw(1, "g-att", &eml, false);
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |_| false);
        let a = &t.attachments;
        assert_eq!(
            a.len(),
            4,
            "pdf + png + cid-inline + big (the text body is not an attachment)"
        );

        let pdf = a.iter().find(|x| x.filename == "doc.pdf").expect("pdf");
        assert_eq!(pdf.mime, "application/pdf");
        assert_eq!(pdf.size_bytes, 5);
        assert_eq!(pdf.data.as_deref(), Some(&b"Hello"[..]));

        let png = a.iter().find(|x| x.filename == "pic.png").expect("png");
        assert_eq!(png.mime, "image/png");
        assert_eq!(png.data.as_deref(), Some(&b"world"[..]));

        // The cid-inline part declares no filename -> attachment-<n> fallback, and
        // its bytes are kept (inline images are how templates embed logos).
        let inline = a
            .iter()
            .find(|x| x.filename.starts_with("attachment-"))
            .expect("cid inline part included");
        assert_eq!(inline.mime, "image/png");
        assert_eq!(inline.data.as_deref(), Some(&b"inline"[..]));

        // The oversized part keeps its metadata (real size) but drops its bytes.
        let big_att = a.iter().find(|x| x.filename == "big.bin").expect("big");
        assert_eq!(big_att.size_bytes, (11 * 1024 * 1024) as i64);
        assert!(
            big_att.data.is_none(),
            "over-cap attachment stores no bytes"
        );
    }

    #[test]
    fn plain_mail_has_no_attachments() {
        let eml = "From: Alice <alice@friends.com>\r\n\
                   Subject: dinner\r\n\
                   Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   friday?\r\n";
        let f = raw(1, "g-plain-att", eml, false);
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |_| false);
        assert!(t.attachments.is_empty());
    }

    #[test]
    fn sealed_mail_still_extracts_attachments_for_storage() {
        // Attachments are STORED for sealed mail like the body; serving is guarded
        // downstream. So extraction must still happen on the sealed path.
        let eml = "From: Bank <noreply@bank.com>\r\n\
                   To: me@example.com\r\n\
                   Subject: Your verification code\r\n\
                   Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
                   MIME-Version: 1.0\r\n\
                   Content-Type: multipart/mixed; boundary=\"B\"\r\n\
                   \r\n\
                   --B\r\nContent-Type: text/plain\r\n\r\n\
                   Your one-time passcode is 483920. Enter this code to continue.\r\n\
                   --B\r\nContent-Type: application/pdf\r\n\
                   Content-Disposition: attachment; filename=\"statement.pdf\"\r\n\
                   Content-Transfer-Encoding: base64\r\n\r\nSGVsbG8=\r\n\
                   --B--\r\n";
        let f = raw(1, "g-sealed-att", eml, false);
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |_| false);
        assert_eq!(t.sensitivity, Sensitivity::Sealed);
        assert_eq!(
            t.attachments.len(),
            1,
            "sealed mail's attachment is still extracted for storage"
        );
        assert_eq!(t.attachments[0].filename, "statement.pdf");
    }

    #[test]
    fn ingest_sent_commits_a_neutral_row_and_is_idempotent() {
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let eml = "From: Me <me@example.com>\r\n\
                   To: Alice <alice@friends.com>\r\n\
                   Subject: Re: Lunch?\r\n\
                   Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   yes, noon works\r\n";
        let now = Utc::now();
        let id = ingest_sent(
            &store,
            acct,
            "me@example.com",
            "sent-1",
            Some("thread-77".to_string()),
            eml.as_bytes().to_vec(),
            Some(now),
            now,
        )
        .unwrap()
        .expect("a normal outbound copy is committed");

        // Visible in the thread, neutral, and never a source of attention noise.
        let view = store.thread_view_with_html(acct, "thread-77").unwrap();
        assert_eq!(view.messages.len(), 1);
        assert!(view.messages[0].content.contains("noon works"));
        let updates = store
            .attention_updates(acct, now - chrono::Duration::days(1), None, None, None)
            .unwrap();
        assert!(
            updates.is_empty(),
            "sent mail never enters the attention bands"
        );
        // Recipients still seed contacts, and ride along on the row itself so the
        // app-send echo shows up in the sent listing addressed to somebody.
        assert!(store.is_known_contact(acct, "alice@friends.com").unwrap());
        let listed = store.sent_listing(acct, 10, 0).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, id);
        assert_eq!(listed[0].to, "Alice <alice@friends.com>");

        // Re-ingesting the same Gmail id upserts onto the same local row.
        let again = ingest_sent(
            &store,
            acct,
            "me@example.com",
            "sent-1",
            Some("thread-77".to_string()),
            eml.as_bytes().to_vec(),
            Some(now),
            now,
        )
        .unwrap();
        assert_eq!(
            again,
            Some(id),
            "idempotent on the UNIQUE(account, gmail id) upsert"
        );
        assert_eq!(
            store
                .thread_view_with_html(acct, "thread-77")
                .unwrap()
                .messages
                .len(),
            1
        );
    }

    #[test]
    fn ingest_sent_skips_a_sealed_outbound_copy_and_commits_nothing() {
        // A reply quoting an OTP trips seal detection (which runs before the
        // `is_sent` branch, by design). Committing that row would make
        // `thread_guard_and_subject` 404 the thread the user is reading, so nothing
        // is written at all.
        let store = SqliteStore::open_in_memory().unwrap();
        let acct = store.ensure_account("me@example.com").unwrap();
        let parent = store
            .upsert_message(&NewMessage {
                account_id: acct,
                gmail_msg_id: "g-parent".to_string(),
                thread_id: "thread-88".to_string(),
                from_addr: "noreply@bank.com".to_string(),
                from_name: None,
                subject: "Your verification code".to_string(),
                received_at: Utc::now(),
                snippet: String::new(),
                body: "hello".to_string(),
                body_html: None,
                is_sent: false,
                to_addrs: None,
                list_unsubscribe: None,
                list_unsub_one_click: false,
                auth_pass: None,
            })
            .unwrap();
        store
            .set_triage(
                parent,
                acct,
                40,
                Tier::Signal,
                Sensitivity::Normal,
                None,
                "",
                "",
                None,
            )
            .unwrap();

        let eml = "From: Me <me@example.com>\r\n\
                   To: Support <support@bank.com>\r\n\
                   Subject: Re: Your verification code\r\n\
                   Date: Mon, 7 Jul 2026 10:00:00 +0000\r\n\
                   \r\n\
                   I never asked for this. Your one-time passcode is 483920.\r\n";
        let now = Utc::now();
        let echoed = ingest_sent(
            &store,
            acct,
            "me@example.com",
            "sent-sealed",
            Some("thread-88".to_string()),
            eml.as_bytes().to_vec(),
            Some(now),
            now,
        )
        .unwrap();
        assert!(echoed.is_none(), "a sealed outbound copy is not echoed");

        // Nothing committed: no sealed row, and the thread still opens with the
        // parent alone rather than 404ing on a sealed member.
        assert!(store.sealed_messages(acct).unwrap().is_empty());
        let view = store.thread_view_with_html(acct, "thread-88").unwrap();
        assert_eq!(
            view.messages.len(),
            1,
            "only the parent; the echo was skipped"
        );
        // Contacts are not seeded either — the whole write was skipped.
        assert!(!store.is_known_contact(acct, "support@bank.com").unwrap());
    }

    #[test]
    fn missing_msgid_falls_back_to_hash() {
        let eml = "From: x@y.com\r\nSubject: hi\r\n\r\nbody\r\n";
        let mut f = raw(1, "", eml, false);
        f.gmail_thread_id = None;
        let t = ingest(&f, &Stage1Config::default(), Utc::now(), |_| false);
        assert!(!t.message.gmail_msg_id.is_empty());
        // thread_id falls back to the derived id, never empty.
        assert!(!t.message.thread_id.is_empty());
    }
}
