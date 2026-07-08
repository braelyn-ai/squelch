//! Sender-rule matching and pattern batteries for Stage-1 triage.
//!
//! Two responsibilities:
//!   1. [`glob_match`]: `*`-wildcard matching of a sender rule's `match_pattern`
//!      against a `from` address (the sync engine's local rules).
//!   2. Static regex batteries for the alert / noise / sales rungs of the
//!      Stage-1 ladder. Kept here so [`super::stage1`] reads as a clean ladder.

use crate::types::SenderRule;
use regex::Regex;
use std::sync::OnceLock;

/// Glob match with `*` wildcards (case-insensitive). `*` matches any run of
/// characters including empty. No other metacharacters are special, so a
/// pattern like `*@newsletter.com` or `billing@*.acme.com` works as expected.
///
/// This is deliberately a hand-rolled matcher rather than translating to regex:
/// user-authored patterns must never be able to inject regex metacharacters.
pub fn glob_match(pattern: &str, candidate: &str) -> bool {
    let pat: Vec<char> = pattern.to_ascii_lowercase().chars().collect();
    let cand: Vec<char> = candidate.to_ascii_lowercase().chars().collect();
    glob_rec(&pat, &cand)
}

fn glob_rec(pat: &[char], cand: &[char]) -> bool {
    // Iterative two-pointer with backtracking on the last '*'.
    let (mut p, mut c) = (0usize, 0usize);
    let (mut star, mut mark): (Option<usize>, usize) = (None, 0);

    while c < cand.len() {
        if p < pat.len() && pat[p] == '*' {
            star = Some(p);
            mark = c;
            p += 1;
        } else if p < pat.len() && pat[p] == cand[c] {
            p += 1;
            c += 1;
        } else if let Some(sp) = star {
            // Backtrack: let the '*' absorb one more char.
            p = sp + 1;
            mark += 1;
            c = mark;
        } else {
            return false;
        }
    }
    while p < pat.len() && pat[p] == '*' {
        p += 1;
    }
    p == pat.len()
}

/// Find the first sender rule whose `match_pattern` glob-matches `from_addr`.
pub fn match_sender_rule<'a>(
    from_addr: &str,
    rules: &'a [SenderRule],
) -> Option<&'a SenderRule> {
    rules.iter().find(|r| glob_match(&r.match_pattern, from_addr))
}

// ---- Static pattern batteries -------------------------------------------

struct Patterns {
    /// Automated-sender address shapes (noreply@, alerts@, ci@, …).
    automated_sender: Vec<Regex>,
    /// Ops/monitoring alert language (build failed, outage, incident, …).
    alert: Vec<Regex>,
    /// Unsubscribe / list / marketing-footer shapes.
    unsubscribe: Vec<Regex>,
    /// Order confirmations / receipts (non-bill).
    receipt: Vec<Regex>,
    /// Cold-outbound / salesy language.
    sales: Vec<Regex>,
}

fn rx(p: &str) -> Regex {
    Regex::new(&format!("(?i){p}")).expect("static rules regex must compile")
}

fn patterns() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| Patterns {
        automated_sender: vec![
            rx(r"\bno[-_.]?reply\b"),
            rx(r"\bdo[-_.]?not[-_.]?reply\b"),
            rx(r"^(alerts?|alerting|ci|build|jenkins|notifications?|monitoring|ops|pagerduty|status|nagios|datadog|sentry)@"),
            rx(r"@(alerts?|notifications?|mailer|bounce|email)\."),
        ],
        alert: vec![
            rx(r"\bbuild (failed|failure|broke|is red)\b"),
            rx(r"\bfailed\b.*\bbuild\b"),
            rx(r"\b(pipeline|job|deploy(ment)?) (failed|failure)\b"),
            rx(r"\boutage\b"),
            rx(r"\bincident\b"),
            rx(r"\bdowntime\b"),
            rx(r"\bservice (disruption|degradation|degraded)\b"),
            rx(r"\bpower outage\b"),
            rx(r"\b(is|are) down\b"),
            rx(r"\bdown(time)? detected\b"),
            rx(r"\b(critical|high[-\s]?severity) alert\b"),
            rx(r"\btriggered\b.*\balert\b"),
            rx(r"\balert\b.*\btriggered\b"),
            rx(r"\bhealth ?check (failed|failing)\b"),
        ],
        unsubscribe: vec![
            rx(r"\bunsubscribe\b"),
            rx(r"\bmanage (your )?(email )?preferences\b"),
            rx(r"\bview (this|in) (email )?(in your )?browser\b"),
            rx(r"\byou('re| are) receiving this (email|because)\b"),
            rx(r"\bupdate your (email )?preferences\b"),
            rx(r"\bopt[-\s]?out\b"),
        ],
        receipt: vec![
            rx(r"\byour order\b"),
            rx(r"\border (confirmation|confirmed|#)\b"),
            rx(r"\breceipt\b"),
            rx(r"\bthanks for your (order|purchase)\b"),
            rx(r"\byour (package|shipment) (has )?(shipped|is on its way)\b"),
            rx(r"\btracking number\b"),
        ],
        sales: vec![
            rx(r"\bbook a (call|demo|meeting)\b"),
            rx(r"\b(schedule|set up) a (call|demo|meeting)\b"),
            rx(r"\bfree (demo|trial|consultation)\b"),
            rx(r"\bquick (call|chat|question)\b"),
            rx(r"\bpricing\b"),
            rx(r"\blimited[-\s]?time (offer|deal)\b"),
            rx(r"\blimited offer\b"),
            rx(r"\bspecial offer\b"),
            rx(r"\b\d+% off\b"),
            rx(r"\bhop on a call\b"),
            rx(r"\bgrab (\d+ )?minutes\b"),
            rx(r"\binterested in learning more\b"),
            rx(r"\bcheck out our\b"),
            rx(r"\bdon'?t miss\b"),
        ],
    })
}

fn any(res: &[Regex], hay: &[&str]) -> bool {
    res.iter().any(|re| hay.iter().any(|h| re.is_match(h)))
}

/// Does this look like it came from an automated/no-reply sender?
pub fn is_automated_sender(from_addr: &str) -> bool {
    patterns().automated_sender.iter().any(|re| re.is_match(from_addr))
}

/// Ops/monitoring alert language present in subject or body?
pub fn is_alert(subject: &str, body: &str) -> bool {
    any(&patterns().alert, &[subject, body])
}

/// Newsletter / marketing / list-mail shape?
pub fn is_unsubscribe_shaped(subject: &str, body: &str) -> bool {
    any(&patterns().unsubscribe, &[subject, body])
}

/// Order confirmation / receipt (non-bill)?
pub fn is_receipt(subject: &str, body: &str) -> bool {
    any(&patterns().receipt, &[subject, body])
}

/// Cold-outbound / salesy language?
pub fn is_sales(subject: &str, body: &str) -> bool {
    any(&patterns().sales, &[subject, body])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_basics() {
        assert!(glob_match("*@newsletter.com", "promo@newsletter.com"));
        assert!(glob_match("*@newsletter.com", "PROMO@Newsletter.COM"));
        assert!(glob_match("billing@*.acme.com", "billing@us.acme.com"));
        assert!(glob_match("*", "anything@x.com"));
        assert!(glob_match("exact@x.com", "exact@x.com"));
        assert!(!glob_match("*@newsletter.com", "alice@example.com"));
        assert!(!glob_match("billing@*.acme.com", "billing@acme.com"));
        assert!(glob_match("a*b*c", "axxbyyc"));
        assert!(!glob_match("a*b*c", "axxbyy"));
    }

    #[test]
    fn glob_never_treats_regex_meta_specially() {
        // '.' is literal, not "any char".
        assert!(!glob_match("a.c", "abc"));
        assert!(glob_match("a.c", "a.c"));
    }

    #[test]
    fn automated_senders() {
        assert!(is_automated_sender("noreply@github.com"));
        assert!(is_automated_sender("no-reply@x.com"));
        assert!(is_automated_sender("ci@buildbot.io"));
        assert!(is_automated_sender("alerts@datadog.com"));
        assert!(!is_automated_sender("alice@example.com"));
    }
}
