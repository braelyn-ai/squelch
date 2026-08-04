//! Markdown → email-safe HTML for the send path. The composer sends the RAW
//! markdown source as the message body (`body_format: "markdown"`); this module
//! renders the HTML alternative that rides beside it in multipart/alternative.
//!
//! The outbound secret guard scans the SOURCE, and this render is the only
//! place the HTML part comes from — so the guard's verdict covers everything
//! that leaves. Nothing here may take HTML from the caller.

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd, html};

/// Schemes a link may keep in outbound mail. Anything else (`javascript:`,
/// `data:`, scheme-relative) renders as its text content, unlinked.
fn safe_link(dest: &str) -> bool {
    let lower = dest.trim().to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("mailto:")
}

/// Render markdown to the HTML body of an email.
///
/// Deviations from stock CommonMark, all deliberate:
/// * Raw HTML NEVER passes through — a typed `<script>` renders as the literal
///   characters. The composer's contract is markdown in, not HTML in.
/// * Soft breaks become hard breaks: people write emails with meaningful line
///   endings, and CommonMark folding "hi\nbob" into one line would mangle them.
/// * Images render as their alt text. Basic formatting only; outbound mail has
///   no business embedding remote resources the recipient auto-fetches.
pub fn render_email_html(source: &str) -> String {
    let parser = Parser::new_ext(source, Options::ENABLE_STRIKETHROUGH);

    // Depth of suppressed link/image tags, so the matching End is swallowed
    // with its Start while the text between them still flows through.
    let mut suppressed: Vec<&'static str> = Vec::new();
    let mut events: Vec<Event> = Vec::new();
    for ev in parser {
        match ev {
            // Raw HTML re-enters as Text, which `push_html` escapes.
            Event::Html(s) | Event::InlineHtml(s) => events.push(Event::Text(s)),
            Event::SoftBreak => events.push(Event::HardBreak),
            Event::Start(Tag::Link { ref dest_url, .. }) if !safe_link(dest_url) => {
                suppressed.push("link");
            }
            Event::End(TagEnd::Link) if suppressed.last() == Some(&"link") => {
                suppressed.pop();
            }
            Event::Start(Tag::Image { .. }) => suppressed.push("image"),
            Event::End(TagEnd::Image) if suppressed.last() == Some(&"image") => {
                suppressed.pop();
            }
            ev => events.push(ev),
        }
    }

    let mut body = String::with_capacity(source.len() * 2);
    html::push_html(&mut body, events.into_iter());

    // Inline styles only — mail clients strip <style> blocks. One wrapper div
    // keeps the mail readable without dictating much.
    format!(
        "<div style=\"font-family:-apple-system,'Segoe UI',Helvetica,Arial,sans-serif;\
         font-size:14px;line-height:1.45\">\n{body}</div>\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_formatting_renders() {
        let html = render_email_html("**bold** and *em* and `code`");
        assert!(html.contains("<strong>bold</strong>"));
        assert!(html.contains("<em>em</em>"));
        assert!(html.contains("<code>code</code>"));
    }

    #[test]
    fn raw_html_is_escaped_not_passed_through() {
        let html = render_email_html("hi <script>alert(1)</script> there");
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn block_html_is_escaped_too() {
        let html = render_email_html("<div onclick=\"x()\">block</div>");
        assert!(!html.contains("<div onclick"));
        assert!(html.contains("&lt;div onclick"));
    }

    #[test]
    fn javascript_links_are_unlinked_text_survives() {
        let html = render_email_html("[click](javascript:alert(1))");
        assert!(!html.contains("javascript:"));
        assert!(!html.contains("<a "));
        assert!(html.contains("click"));
    }

    #[test]
    fn http_links_keep_their_href() {
        let html = render_email_html("[site](https://example.com)");
        assert!(html.contains("<a href=\"https://example.com\">site</a>"));
    }

    #[test]
    fn soft_breaks_become_br() {
        let html = render_email_html("hi\nbob");
        assert!(html.contains("hi<br />\nbob") || html.contains("hi<br>\nbob"));
    }

    #[test]
    fn images_render_as_alt_text() {
        let html = render_email_html("![a chart](https://example.com/x.png)");
        assert!(!html.contains("<img"));
        assert!(html.contains("a chart"));
    }

    #[test]
    fn headings_and_lists_render() {
        let html = render_email_html("# Title\n\n- one\n- two");
        assert!(html.contains("<h1>Title</h1>"));
        assert!(html.contains("<li>one</li>"));
    }
}
