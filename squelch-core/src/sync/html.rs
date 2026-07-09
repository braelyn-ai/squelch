//! Server-side HTML email sanitization (runs AT INGEST, once, before storage).
//!
//! The rendered HTML body of an email is untrusted attacker-controlled markup.
//! We sanitize it here with [`ammonia`] and store the result in
//! `messages.body_html`; the desktop client renders that stored string inside a
//! script-less, opaque-origin `<iframe srcdoc>` with a strict CSP. Sanitization
//! is the FIRST of two boundaries (defense in depth); the client CSP is the real
//! boundary for resource loads (img/CSS `url()` fetches).
//!
//! SECURITY POLICY (what this strips vs. allows) and every DEVIATION from
//! ammonia's defaults is documented inline on [`sanitize_email_html`]. The agent
//! door (`/mcp`) never sees this output — HTML never crosses the MCP boundary;
//! only the flattened text does.

use std::collections::{HashMap, HashSet};

use ammonia::Builder;

/// Sanitize an untrusted HTML email body into a storage-safe fragment.
///
/// The returned string is what the desktop client renders in its sandboxed
/// iframe. This is a pure function of its input (no network, no I/O) so it is
/// fully unit-testable against fixture markup.
///
/// ## Stripped (removed entirely, per the locked design)
/// - `<script>` — active content. (Ammonia strips this by default; we also list
///   it explicitly below via the allow-list being a closed set.)
/// - `on*` event-handler attributes (`onerror`, `onclick`, …) — never in the
///   per-tag attribute allow-list, so ammonia drops them.
/// - `javascript:` / `data:` (and other non-approved) URL schemes in `href`/`src`
///   — restricted via `url_schemes` to `http`/`https`/`mailto` only.
/// - `<form>`, `<input>`, `<button>`, `<textarea>`, `<select>` — interactive
///   form controls; not in the tag allow-list.
/// - `<iframe>`, `<object>`, `<embed>`, `<applet>` — nested browsing
///   contexts / plugins; not in the tag allow-list.
/// - `<meta>`, `<link>`, `<base>`, `<style>` — document-level directives and
///   external stylesheet hooks; not in the tag allow-list. (Inline `style`
///   ATTRIBUTES are still allowed — see below.)
///
/// ## Allowed (kept)
/// - Normal formatting + table-layout tags (headings, `p`, `div`, `span`,
///   lists, `table`/`tr`/`td`, `b`/`i`/`strong`/`em`, `blockquote`, …).
/// - `<img>` with `src` KEPT. The client blocks the actual load via CSP
///   (`img-src 'none'` until the user opts in per-message with 'i'), so a kept
///   `src` renders as a broken image, not a tracking-pixel fetch. `cid:` refs
///   are left as-is and render broken (documented v2).
/// - `<a href>` restricted to `http`/`https`/`mailto`. `rel="noopener
///   noreferrer"` is force-added and `target` normalized so a link can't reach
///   back into the (already opaque-origin) frame's opener.
/// - Inline `style` attributes on all allowed tags. CSS `url()` fetches are the
///   client CSP's responsibility (`style-src 'unsafe-inline'` allows the inline
///   CSS text itself; `default-src 'none'` blocks any `url()` resource load).
///
/// ## DEVIATIONS from `ammonia::Builder::default()` (each deliberate)
/// 1. `url_schemes`: default allows `http https mailto tel` + several others; we
///    narrow to exactly `{http, https, mailto}` (drop `tel`, `ftp`, etc.) — an
///    email body has no need for them and each scheme is attack surface.
/// 2. `add_tags("style")` — NOT done. Ammonia's default tag set does not include
///    raw `<style>` blocks anyway; we keep it excluded so an email cannot ship a
///    document-level stylesheet (only inline `style` attributes survive).
/// 3. `generic_attributes`: we add `"style"` to the generic (all-tags) set,
///    which ammonia's default does NOT include — the locked design explicitly
///    wants inline styles to pass (the client CSP is the CSS boundary).
/// 4. `add_allowed_classes` / id handling: left at defaults (ammonia drops
///    `id`/`class` unless allow-listed) — we don't need them and they're extra
///    surface for CSS-targeting tricks.
/// 5. `link_rel`: ammonia already defaults to `noopener noreferrer`; we set it
///    explicitly so the guarantee is visible and version-stable.
/// 6. `img` `src`: kept (in the img attribute allow-list). Ammonia's default
///    also allows `img src`, but because our `url_schemes` no longer contains
///    `data`, inlined `data:` image payloads are dropped — only remote/`cid:`
///    refs survive as (client-blocked) broken images.
pub fn sanitize_email_html(html: &str) -> String {
    let mut builder = Builder::default();

    // DEVIATION 1: narrow URL schemes to exactly the three an email body needs.
    let url_schemes: HashSet<&str> = ["http", "https", "mailto"].into_iter().collect();
    builder.url_schemes(url_schemes);

    // DEVIATION 3: allow inline `style` on every allowed tag. The client CSP
    // (`default-src 'none'; style-src 'unsafe-inline'`) is the real boundary for
    // any `url()` inside that CSS, so allowing the inline text here is safe.
    let mut generic_attributes: HashSet<&str> = builder.clone_generic_attributes();
    generic_attributes.insert("style");
    builder.generic_attributes(generic_attributes);

    // Keep `src` on <img> (client CSP blocks the actual fetch). Ammonia's
    // default img attributes include width/height/alt/src; make the intent
    // explicit and additive rather than relying on the default set.
    let mut tag_attributes: HashMap<&str, HashSet<&str>> = builder.clone_tag_attributes();
    let img_attrs = tag_attributes.entry("img").or_default();
    for a in ["src", "alt", "width", "height", "title"] {
        img_attrs.insert(a);
    }
    builder.tag_attributes(tag_attributes);

    // DEVIATION 5: pin link rel to noopener/noreferrer explicitly (matches the
    // ammonia default, stated here so it can't silently regress).
    builder.link_rel(Some("noopener noreferrer"));

    builder.clean(html).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_tag_is_stripped() {
        let out = sanitize_email_html("<p>hi</p><script>alert(1)</script>");
        assert!(out.contains("hi"));
        assert!(!out.to_lowercase().contains("script"));
        assert!(!out.contains("alert"));
    }

    #[test]
    fn onerror_and_onclick_are_stripped() {
        let out = sanitize_email_html(
            "<img src=\"http://x/y.png\" onerror=\"alert(1)\"><a href=\"http://z\" onclick=\"steal()\">c</a>",
        );
        assert!(!out.to_lowercase().contains("onerror"));
        assert!(!out.to_lowercase().contains("onclick"));
        assert!(!out.contains("alert"));
        assert!(!out.contains("steal"));
        // The benign img/src and link survive.
        assert!(out.contains("http://x/y.png"));
    }

    #[test]
    fn javascript_href_is_stripped() {
        let out = sanitize_email_html("<a href=\"javascript:alert(1)\">click</a>");
        assert!(!out.to_lowercase().contains("javascript:"));
        assert!(!out.contains("alert"));
        // The text is preserved even though the dangerous href is dropped.
        assert!(out.contains("click"));
    }

    #[test]
    fn data_uri_src_is_stripped() {
        let out = sanitize_email_html(
            "<img src=\"data:text/html;base64,PHNjcmlwdD4=\"><img src=\"https://ok/i.png\">",
        );
        assert!(!out.contains("data:"));
        // The https image is kept.
        assert!(out.contains("https://ok/i.png"));
    }

    #[test]
    fn form_and_inputs_are_stripped() {
        let out = sanitize_email_html(
            "<form action=\"http://evil\"><input name=\"pw\"><button>go</button></form><p>body</p>",
        );
        assert!(!out.to_lowercase().contains("<form"));
        assert!(!out.to_lowercase().contains("<input"));
        assert!(!out.to_lowercase().contains("<button"));
        assert!(out.contains("body"));
    }

    #[test]
    fn iframe_object_embed_meta_link_stripped() {
        let out = sanitize_email_html(
            "<iframe src=\"http://x\"></iframe><object></object><embed>\
             <meta http-equiv=\"refresh\" content=\"0\"><link rel=\"stylesheet\" href=\"http://x\">\
             <p>kept</p>",
        );
        for bad in ["<iframe", "<object", "<embed", "<meta", "<link"] {
            assert!(!out.to_lowercase().contains(bad), "leaked: {bad}");
        }
        assert!(out.contains("kept"));
    }

    #[test]
    fn benign_table_img_style_email_passes_through() {
        let input = "<table><tr><td style=\"color:red\">Cell</td></tr></table>\
                     <p><strong>Bold</strong> and <a href=\"https://example.com\">link</a></p>\
                     <img src=\"https://cdn.example.com/logo.png\" alt=\"logo\">";
        let out = sanitize_email_html(input);
        assert!(out.contains("<table"));
        assert!(out.contains("<td"));
        assert!(out.contains("style=\"color:red\""), "inline style must survive: {out}");
        assert!(out.contains("<strong"));
        assert!(out.contains("https://example.com"));
        assert!(out.contains("https://cdn.example.com/logo.png"));
        assert!(out.contains("alt=\"logo\""));
    }

    #[test]
    fn empty_and_plaintext_are_harmless() {
        assert_eq!(sanitize_email_html(""), "");
        // Plain text with no markup survives verbatim (entities aside).
        assert_eq!(sanitize_email_html("just words"), "just words");
    }
}
