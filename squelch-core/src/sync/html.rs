//! Server-side HTML email sanitization: runs once at ingest, before storage.
//!
//! Email HTML is untrusted markup; [`ammonia`] cleans it into `messages.body_html`.
//! Defense in depth only — the client CSP is the real boundary for resource loads,
//! and HTML never crosses the MCP boundary (the agent door sees flattened text).
//! See docs/SECURITY.md.

use std::collections::{HashMap, HashSet};

use ammonia::Builder;

/// Sanitize an untrusted HTML email body into a storage-safe fragment. Pure (no
/// I/O). Kept: formatting/table tags, `<img src>`, `<a href>` limited to
/// http/https/mailto, `<style>` blocks with their CSS verbatim, and
/// `style`/`class`/`id` on any allowed tag. Dropped: `<script>`, `on*` handlers,
/// form controls, frames/plugins, `<meta>`/`<link>`/`<base>`, and every URL scheme
/// outside `{http, https, mailto}` (so inline `data:` payloads die too). Full
/// policy, and the client CSP that is the real boundary for what CSS and images
/// can fetch: docs/SECURITY.md.
///
/// INVARIANT: ammonia escapes `<` in text content AND in attribute values, so
/// every `<img`/`<style` that survives here is a real tag — the client's
/// regex-based image rewrites depend on it. Do not add a raw-text tag beyond
/// `<style>`, and do not disable that escaping.
pub fn sanitize_email_html(html: &str) -> String {
    let mut builder = Builder::default();

    // Narrow to the three schemes an email body needs; dropping `data:` here is
    // what kills inlined `data:` image payloads.
    let url_schemes: HashSet<&str> = ["http", "https", "mailto"].into_iter().collect();
    builder.url_schemes(url_schemes);

    // Keep `<style>` blocks with their CSS verbatim (class-styled newsletters are
    // illegible without it). `style` MUST leave `clean_content_tags` before being
    // added as an allowed tag or ammonia panics on the conflicting instruction;
    // html5ever then serializes it as raw text, so the CSS is not entity-escaped.
    builder.rm_clean_content_tags(&["style"]);
    builder.add_tags(&["style"]);

    // Inline `style` on every allowed tag, plus the `class`/`id` hooks the document
    // stylesheet's selectors need. The client CSP (`default-src 'none'; style-src
    // 'unsafe-inline'`) is the real boundary for any `url()`/`@import` in that CSS.
    let mut generic_attributes: HashSet<&str> = builder.clone_generic_attributes();
    generic_attributes.insert("style");
    generic_attributes.insert("class");
    generic_attributes.insert("id");
    builder.generic_attributes(generic_attributes);

    // Keep `src` on <img>: the client rewrites it to its own proxy scheme and its
    // CSP is what gates the fetch. Listed explicitly rather than inherited.
    let mut tag_attributes: HashMap<&str, HashSet<&str>> = builder.clone_tag_attributes();
    let img_attrs = tag_attributes.entry("img").or_default();
    for a in ["src", "alt", "width", "height", "title"] {
        img_attrs.insert(a);
    }
    builder.tag_attributes(tag_attributes);

    // Pinned explicitly (it is also the ammonia default) so it can't silently
    // regress: a link must never reach back into the frame's opener.
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
        assert!(out.contains("http://x/y.png"));
    }

    #[test]
    fn javascript_href_is_stripped() {
        let out = sanitize_email_html("<a href=\"javascript:alert(1)\">click</a>");
        assert!(!out.to_lowercase().contains("javascript:"));
        assert!(!out.contains("alert"));
        // The text survives even though the dangerous href is dropped.
        assert!(out.contains("click"));
    }

    #[test]
    fn data_uri_src_is_stripped() {
        let out = sanitize_email_html(
            "<img src=\"data:text/html;base64,PHNjcmlwdD4=\"><img src=\"https://ok/i.png\">",
        );
        assert!(!out.contains("data:"));
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
    fn style_block_survives_with_css_verbatim() {
        // The CSS must come through UNESCAPED: `>` combinators and quoted font
        // names break if the serializer entity-escapes the text.
        let out = sanitize_email_html(
            "<style>.wrap > a { color: #ffffff; } td { font-family: \"SF Pro\"; }</style>\
             <div class=\"wrap\" id=\"outer\"><a href=\"https://x\">link</a></div>",
        );
        assert!(out.contains("<style>"), "style block must survive: {out}");
        assert!(out.contains(".wrap > a { color: #ffffff; }"), "css must be verbatim: {out}");
        assert!(out.contains("font-family: \"SF Pro\""), "quotes must not be escaped: {out}");
        assert!(out.contains("class=\"wrap\""), "class must survive for selectors: {out}");
        assert!(out.contains("id=\"outer\""), "id must survive for selectors: {out}");
    }

    #[test]
    fn script_stays_fully_stripped_despite_style_allowance() {
        // Loosening `style` out of clean_content_tags must not loosen script:
        // both the element and its TEXT still vanish.
        let out = sanitize_email_html("<style>p{color:red}</style><script>steal()</script>");
        assert!(out.contains("p{color:red}"));
        assert!(!out.to_lowercase().contains("script"));
        assert!(!out.contains("steal"));
    }

    #[test]
    fn empty_and_plaintext_are_harmless() {
        assert_eq!(sanitize_email_html(""), "");
        assert_eq!(sanitize_email_html("just words"), "just words");
    }
}
