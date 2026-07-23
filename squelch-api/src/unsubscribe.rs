//! Unsubscribe support for the human door: `List-Unsubscribe` header parsing and
//! http(s) URL selection.
//!
//! The server NEVER performs the unsubscribe itself. The client confirms with the
//! user and opens the returned URL in the user's browser. This module is therefore
//! PURE (no network, no host resolution): it extracts the first http(s) URL from a
//! message's stored `List-Unsubscribe` header, or `None` when there is none.
//!
//! The RFC 8058 one-click flag (`List-Unsubscribe-Post`) is ignored for selection
//! — we no longer distinguish one-click from a plain link — though ingest keeps
//! storing it for possible future use. Only `http`/`https` URLs are ever returned;
//! `mailto:` and non-web schemes are discarded.

/// The unsubscribe outcome for a message, derived purely from its stored
/// `List-Unsubscribe` header value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnsubPlan {
    /// An http(s) URL the CLIENT opens in the user's browser.
    Browser { url: String },
    /// No usable http(s) unsubscribe URL (=> 422).
    None,
}

/// Select the first http(s) unsubscribe URL from the raw header value. The
/// one-click flag is irrelevant to selection and is not a parameter. Returns
/// [`UnsubPlan::Browser`] with that URL, or [`UnsubPlan::None`] when the header is
/// absent or carries no http(s) URL. PURE (no network) so it is fully unit-testable.
pub fn classify_unsubscribe(list_unsubscribe: Option<&str>) -> UnsubPlan {
    let Some(header) = list_unsubscribe else {
        return UnsubPlan::None;
    };
    match first_http_url(header) {
        Some(url) => UnsubPlan::Browser { url },
        None => UnsubPlan::None,
    }
}

/// Return the first `http`/`https` URL among the `<…>`-bracketed entries of a
/// `List-Unsubscribe` value. Non-web schemes (`mailto:`, `ftp:`, `javascript:`,
/// …) and anything outside the brackets are ignored, so the caller never hands a
/// non-web scheme to the client.
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
}
