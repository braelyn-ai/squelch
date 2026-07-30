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
}
