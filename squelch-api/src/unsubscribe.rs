//! `List-Unsubscribe` parsing for the human door.
//!
//! The server NEVER performs the unsubscribe itself — the client confirms with
//! the user and opens the URL — so this module is PURE (no network, no host
//! resolution). Only `http`/`https` URLs are ever returned; the RFC 8058
//! one-click flag is not part of the selection.

/// The unsubscribe outcome for a message, derived purely from its stored
/// `List-Unsubscribe` header value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnsubPlan {
    /// An http(s) URL the CLIENT opens in the user's browser.
    Browser { url: String },
    /// No usable http(s) unsubscribe URL (=> 422).
    None,
}

/// Select the first http(s) unsubscribe URL from the raw header value, or
/// [`UnsubPlan::None`] when the header is absent or carries none.
pub fn classify_unsubscribe(list_unsubscribe: Option<&str>) -> UnsubPlan {
    let Some(header) = list_unsubscribe else {
        return UnsubPlan::None;
    };
    match first_http_url(header) {
        Some(url) => UnsubPlan::Browser { url },
        None => UnsubPlan::None,
    }
}

/// Anchor phrases that mean "stop sending me this", matched against a link's
/// TEXT. Kept short and unambiguous: every extra phrase is another chance to
/// hand the reader a link they did not ask for.
const UNSUB_ANCHORS: &[&str] = &[
    "unsubscribe",
    "opt out",
    "opt-out",
    "manage preferences",
    "email preferences",
    "manage your preferences",
    "stop receiving",
];

/// Longest URL worth handing back. Tracking redirects are genuinely long — the
/// one that prompted this is 787 characters — so the cap is generous, and only
/// there to bound what a hostile body can push through.
const MAX_URL_LEN: usize = 4096;

/// FOOTER-LINK FALLBACK: the first http(s) link in the body whose own TEXT says
/// it unsubscribes. For the many senders that ship no `List-Unsubscribe` header
/// and put the link in the mail instead.
///
/// MATCHED ON THE LINK TEXT, NEVER THE URL. An unsubscribe endpoint is usually a
/// tracking redirect with nothing recognisable in it — the message that prompted
/// this has 787 characters of click-tracking and not one occurrence of "unsub" —
/// while the words a human reads are exactly the signal. That same document
/// carries 23 links, so an ungated scan would be a coin flip.
///
/// THE TRUST STORY: this URL is attacker-chosen. So is the header's, which is
/// why this does not widen the capability — the sender could always have named
/// any URL. What it widens is OUR chance of picking the wrong one, and the
/// anchor gate plus the http(s) filter are what hold that down. The server still
/// never fetches it; the client confirms and the URL opens in the reader's own
/// browser, where the address bar is the last check.
pub fn classify_unsubscribe_body(body_html: Option<&str>) -> UnsubPlan {
    let Some(html) = body_html else {
        return UnsubPlan::None;
    };
    for (href, text) in anchors(html) {
        let lower_text = text.to_ascii_lowercase();
        if !UNSUB_ANCHORS.iter().any(|p| lower_text.contains(p)) {
            continue;
        }
        let url = decode_entities(href.trim());
        let lower_url = url.to_ascii_lowercase();
        if (lower_url.starts_with("http://") || lower_url.starts_with("https://"))
            && url.len() <= MAX_URL_LEN
        {
            return UnsubPlan::Browser { url };
        }
    }
    UnsubPlan::None
}

/// Every `<a href=…>text</a>` as a (href, text) pair, tags stripped out of the
/// text so `<a><span>unsubscribe</span></a>` still reads as "unsubscribe".
fn anchors(html: &str) -> Vec<(String, String)> {
    let lower = html.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(rel) = lower[i..].find("<a") {
        let tag_start = i + rel;
        // "<a" must begin a tag, not merely prefix a longer name ("<abbr").
        let after = lower.as_bytes().get(tag_start + 2).copied();
        if !matches!(after, Some(b' ') | Some(b'\t') | Some(b'\n') | Some(b'\r')) {
            i = tag_start + 2;
            continue;
        }
        let Some(rel_gt) = lower[tag_start..].find('>') else { break };
        let tag_end = tag_start + rel_gt;
        let href = attr_value(&html[tag_start..tag_end], &lower[tag_start..tag_end], "href");
        // Unclosed anchors are skipped rather than run to end-of-document.
        let Some(rel_close) = lower[tag_end..].find("</a") else { break };
        let close = tag_end + rel_close;
        if let Some(href) = href {
            out.push((href, strip_tags(&html[tag_end + 1..close])));
        }
        i = close + 3;
    }
    out
}

/// One attribute's value from inside a tag, single- or double-quoted. Unquoted
/// values are ignored: they cannot hold a URL with a query string anyway.
fn attr_value(raw: &str, lower: &str, name: &str) -> Option<String> {
    let mut from = 0;
    while let Some(rel) = lower[from..].find(name) {
        let at = from + rel;
        // Must be preceded by whitespace, so "data-href" never matches "href".
        let ok_before = at == 0
            || matches!(lower.as_bytes()[at - 1], b' ' | b'\t' | b'\n' | b'\r' | b'"' | b'\'');
        let rest = lower[at + name.len()..].trim_start();
        if ok_before && rest.starts_with('=') {
            let eq = lower[at + name.len()..].find('=')? + at + name.len();
            let after = lower[eq + 1..].trim_start();
            let offset = eq + 1 + (lower[eq + 1..].len() - after.len());
            let quote = after.chars().next()?;
            if quote == '"' || quote == '\'' {
                let end = raw[offset + 1..].find(quote)? + offset + 1;
                return Some(raw[offset + 1..end].to_string());
            }
            return None;
        }
        from = at + name.len();
    }
    None
}

/// Text content with any nested markup removed, whitespace collapsed.
fn strip_tags(fragment: &str) -> String {
    let mut out = String::with_capacity(fragment.len());
    let mut depth = 0usize;
    for ch in fragment.chars() {
        match ch {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(ch),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The handful of entities that actually appear in a sanitized href. `&amp;` is
/// the load-bearing one: every query string carries it, and opening a URL with
/// a literal "&amp;" in place of "&" silently breaks the unsubscribe.
fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&#38;", "&")
        .replace("&#x26;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

/// The first `http`/`https` URL among the `<…>`-bracketed entries. Non-web
/// schemes and anything outside the brackets are ignored, so a non-web scheme
/// can never reach the client.
fn first_http_url(header: &str) -> Option<String> {
    let bytes = header.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<'
            && let Some(end) = header[i + 1..].find('>')
        {
            let inner = header[i + 1..i + 1 + end].trim();
            let lower = inner.to_ascii_lowercase();
            if lower.starts_with("http://") || lower.starts_with("https://") {
                return Some(inner.to_string());
            }
            i = i + 1 + end + 1;
            continue;
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_http_url_selected_past_mailto() {
        let h = "<mailto:u@x.com>, <https://x.com/u/1>";
        assert_eq!(
            classify_unsubscribe(Some(h)),
            UnsubPlan::Browser { url: "https://x.com/u/1".into() }
        );
    }

    #[test]
    fn http_scheme_is_accepted() {
        let h = "<http://x.com/u/1>";
        assert_eq!(
            classify_unsubscribe(Some(h)),
            UnsubPlan::Browser { url: "http://x.com/u/1".into() }
        );
    }

    #[test]
    fn first_of_several_urls_wins() {
        let h = "<https://a.com/1>, <https://b.com/2>";
        assert_eq!(
            classify_unsubscribe(Some(h)),
            UnsubPlan::Browser { url: "https://a.com/1".into() }
        );
    }

    #[test]
    fn mailto_only_is_none() {
        // No http(s) URL to hand the client => 422 at the handler.
        let h = "<mailto:u@x.com?subject=Bye>";
        assert_eq!(classify_unsubscribe(Some(h)), UnsubPlan::None);
    }

    #[test]
    fn non_web_scheme_is_ignored() {
        let h = "<ftp://x.com/u>, <javascript:alert(1)>";
        assert_eq!(classify_unsubscribe(Some(h)), UnsubPlan::None);
    }

    #[test]
    fn none_when_absent_or_blank() {
        assert_eq!(classify_unsubscribe(None), UnsubPlan::None);
        assert_eq!(classify_unsubscribe(Some("   ")), UnsubPlan::None);
    }

    // ---- footer-link fallback ------------------------------------------

    fn url_of(plan: UnsubPlan) -> Option<String> {
        match plan {
            UnsubPlan::Browser { url } => Some(url),
            UnsubPlan::None => None,
        }
    }

    /// THE SHAPE THAT PROMPTED THIS: a long tracking redirect with nothing
    /// recognisable in the URL, among many other links, identified only by the
    /// word a human reads.
    #[test]
    fn footer_link_is_found_by_its_text_not_its_url() {
        let html = r#"
            <a href="https://shop.example/sale">Shop the sale</a>
            <a href="https://shop.example/account">Your account</a>
            <a href="https://click.eu.marketing.leat.com/ls/click?upn=u001.Qq7kQ8bFqS4"
               style="color:#FFF" rel="noopener noreferrer">unsubscribe</a> from our list.
        "#;
        assert_eq!(
            url_of(classify_unsubscribe_body(Some(html))).as_deref(),
            Some("https://click.eu.marketing.leat.com/ls/click?upn=u001.Qq7kQ8bFqS4"),
            "the link whose TEXT says unsubscribe, not the first link in the document"
        );
    }

    /// Query strings arrive HTML-escaped; a literal "&amp;" would break the URL.
    #[test]
    fn ampersands_in_the_href_are_decoded() {
        let html = r#"<a href="https://x.com/u?a=1&amp;b=2">Unsubscribe</a>"#;
        assert_eq!(
            url_of(classify_unsubscribe_body(Some(html))).as_deref(),
            Some("https://x.com/u?a=1&b=2")
        );
    }

    /// Markup inside the anchor must not hide the word from the match.
    #[test]
    fn nested_markup_inside_the_anchor_still_reads() {
        let html = r#"<a href="https://x.com/u"><span><b>Un</b>subscribe</span> here</a>"#;
        assert_eq!(
            url_of(classify_unsubscribe_body(Some(html))).as_deref(),
            Some("https://x.com/u")
        );
    }

    /// NON-WEB SCHEMES NEVER REACH THE CLIENT, exactly as for the header path —
    /// and a hostile body is the likeliest place to find one.
    #[test]
    fn non_http_schemes_are_refused() {
        for href in [
            "javascript:alert(1)",
            "data:text/html,<script>alert(1)</script>",
            "file:///etc/passwd",
            "mailto:unsub@x.com",
        ] {
            let html = format!(r#"<a href="{href}">unsubscribe</a>"#);
            assert_eq!(
                classify_unsubscribe_body(Some(&html)),
                UnsubPlan::None,
                "{href} must not be handed to the client"
            );
        }
    }

    /// An ordinary link is never mistaken for an unsubscribe, however many there
    /// are: without the anchor gate this is a coin flip across 23 links.
    #[test]
    fn links_that_do_not_say_so_are_ignored() {
        let html = r#"
            <a href="https://x.com/a">Home</a>
            <a href="https://x.com/b">View in browser</a>
            <a href="https://x.com/c"><img src="cid:1"></a>
        "#;
        assert_eq!(classify_unsubscribe_body(Some(html)), UnsubPlan::None);
    }

    /// `data-href` is not `href`, and `<abbr>` is not `<a>`.
    #[test]
    fn lookalike_attributes_and_tags_do_not_match() {
        let html = r#"
            <abbr title="https://evil.com">unsubscribe</abbr>
            <a data-href="https://evil.com">unsubscribe</a>
        "#;
        assert_eq!(classify_unsubscribe_body(Some(html)), UnsubPlan::None);
    }

    #[test]
    fn absent_body_and_absurd_urls_are_none() {
        assert_eq!(classify_unsubscribe_body(None), UnsubPlan::None);
        let long = format!(r#"<a href="https://x.com/{}">unsubscribe</a>"#, "a".repeat(5000));
        assert_eq!(classify_unsubscribe_body(Some(&long)), UnsubPlan::None);
    }

}
