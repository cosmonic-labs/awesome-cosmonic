//! Markdown/HTML sanitization — pure compute, no I/O.
//!
//! Two entry points turn untrusted input into safe HTML entirely on-device:
//!
//! - [`sanitize_html`] runs raw HTML through [`ammonia`]'s allowlist-based
//!   cleaner, which parses with html5ever and keeps only a safe subset of tags
//!   and attributes. It strips `<script>`/`<style>`/`<iframe>`/`<object>`/
//!   `<embed>`, every event-handler attribute (`onclick`, `onerror`, …), and
//!   dangerous URL schemes (`javascript:`, and `data:` on href/src).
//! - [`render_markdown`] renders CommonMark to HTML with [`pulldown_cmark`],
//!   then passes the rendered HTML **back through ammonia**. CommonMark permits
//!   raw inline HTML, so any embedded `<script>` in the markdown is neutralized
//!   by the same allowlist rather than passed through.
//!
//! ## Why a crate, not a hand-rolled sanitizer
//!
//! Hand-rolled HTML sanitizers leak XSS. Sanitization is done with the
//! battle-tested [`ammonia`] crate (html5ever tokenizer + allowlist), never by
//! ad-hoc string munging.

/// Maximum input size accepted by either entry point (256 KiB). Larger inputs
/// return [`SanitizeError::TooLarge`] rather than being processed.
pub const MAX_INPUT_BYTES: usize = 256 * 1024;

/// Why a sanitize/render call refused to run.
#[derive(Debug, thiserror::Error)]
pub enum SanitizeError {
    /// Input exceeded [`MAX_INPUT_BYTES`].
    #[error("input is {actual} bytes, which exceeds the {limit}-byte limit")]
    TooLarge { limit: usize, actual: usize },
}

fn check_size(input: &str) -> Result<(), SanitizeError> {
    if input.len() > MAX_INPUT_BYTES {
        return Err(SanitizeError::TooLarge {
            limit: MAX_INPUT_BYTES,
            actual: input.len(),
        });
    }
    Ok(())
}

/// The result of a successful HTML sanitization pass.
#[derive(Debug)]
pub struct HtmlResult {
    /// The input with everything outside ammonia's allowlist removed.
    pub sanitized: String,
    /// Whether the sanitizer changed the input (output != input). True when any
    /// tag/attribute/URL was stripped or the markup was rewritten.
    pub removed: bool,
}

/// Sanitize raw HTML with ammonia's default allowlist.
///
/// Returns the cleaned HTML plus a `removed` flag that is true when the output
/// differs from the input (i.e. something was stripped or rewritten).
pub fn sanitize_html(html: &str) -> Result<HtmlResult, SanitizeError> {
    check_size(html)?;
    let sanitized = ammonia::clean(html);
    let removed = sanitized != html;
    Ok(HtmlResult { sanitized, removed })
}

/// The result of a successful markdown render.
#[derive(Debug)]
pub struct MarkdownResult {
    /// The rendered, sanitized HTML.
    pub html: String,
}

/// Render CommonMark to HTML, then sanitize the result.
///
/// The markdown is rendered with pulldown-cmark (raw inline HTML enabled, as
/// CommonMark allows), and the rendered HTML is passed through ammonia so any
/// embedded raw HTML — `<script>` and friends — is neutralized by the same
/// allowlist that guards [`sanitize_html`].
pub fn render_markdown(markdown: &str) -> Result<MarkdownResult, SanitizeError> {
    check_size(markdown)?;
    let parser = pulldown_cmark::Parser::new(markdown);
    let mut rendered = String::with_capacity(markdown.len());
    pulldown_cmark::html::push_html(&mut rendered, parser);
    let html = ammonia::clean(&rendered);
    Ok(MarkdownResult { html })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_script_handlers_and_js_url() {
        let r = sanitize_html(
            "<p onclick=alert(1)>Hi</p><script>steal()</script>\
             <a href=\"javascript:evil()\">x</a><img src=x onerror=alert(1)>",
        )
        .unwrap();
        assert!(r.removed);
        assert!(!r.sanitized.contains("<script"));
        assert!(!r.sanitized.contains("steal()"));
        assert!(!r.sanitized.contains("onclick"));
        assert!(!r.sanitized.contains("onerror"));
        assert!(!r.sanitized.contains("javascript:"));
        // Safe structure and text survive.
        assert!(r.sanitized.contains("<p>Hi</p>"));
        assert!(r.sanitized.contains(">x</a>"));
    }

    #[test]
    fn safe_html_may_be_normalized_but_kept() {
        let r = sanitize_html("<p>Hello <strong>world</strong></p>").unwrap();
        assert!(r.sanitized.contains("<strong>world</strong>"));
        assert!(r.sanitized.contains("Hello"));
    }

    #[test]
    fn markdown_renders_and_neutralizes_raw_script() {
        let r = render_markdown("# Hi\n\nnormal **bold** and a raw <script>alert(1)</script> tag")
            .unwrap();
        assert!(r.html.contains("<h1>Hi</h1>"));
        assert!(r.html.contains("<strong>bold</strong>"));
        assert!(!r.html.contains("<script"));
        assert!(!r.html.contains("alert(1)"));
    }

    #[test]
    fn oversized_input_errors() {
        let big = "a".repeat(MAX_INPUT_BYTES + 1);
        assert!(matches!(
            sanitize_html(&big).unwrap_err(),
            SanitizeError::TooLarge { .. }
        ));
        assert!(matches!(
            render_markdown(&big).unwrap_err(),
            SanitizeError::TooLarge { .. }
        ));
    }
}
