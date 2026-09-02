//! Body-format conversion for Confluence content.
//!
//! Confluence stores page bodies as **storage format** — XHTML with
//! `<ac:*>`/`<ri:*>` macro elements — and can also hand out **Atlassian
//! Document Format** (ADF, a JSON tree). Neither is pleasant for an agent to
//! read or write, so this module provides three lossy-but-predictable
//! conversions:
//!
//! - [`storage_to_text`] — storage XHTML → readable markdown-ish text
//!   (headings, lists, tables, code macros as fences, links, macro markers);
//! - [`adf_to_text`] — ADF JSON → the same text dialect;
//! - [`markdown_to_storage`] — CommonMark (+ tables, strikethrough, task
//!   lists) → well-formed storage XHTML, with fenced code blocks emitted as
//!   Confluence code macros.
//!
//! Everything here is bounded: the tokenizer never recurses, nesting depth is
//! capped, output buffers stop growing past a hard limit, and no slice is
//! ever taken off a character boundary. A malformed body produces odd text,
//! never a panic.
//!
//! Storage format is XML 1.0, which forbids most control characters even
//! inside CDATA — so [`xml_escape`] and the CDATA writer drop every char the
//! grammar disallows (`U+0000..=U+0008`, `U+000B`, `U+000C`,
//! `U+000E..=U+001F`, `U+007F`, `U+FFFE`, `U+FFFF`), and [`strip_xml_illegal`]
//! lets callers apply the same filter to titles and messages. Pasted terminal
//! output with ANSI escapes therefore loses its `ESC` bytes instead of
//! earning a `400 Error parsing xhtml` from Confluence.

use std::fmt::Write as _;

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

/// Marker appended to any text this module truncates.
pub const TRUNCATED: &str = "…[truncated]";

/// Hard cap on a rendered text buffer (bytes) regardless of the caller's
/// `max_chars`; keeps a pathological body from ballooning in memory.
const RENDER_CAP: usize = 4 * 1024 * 1024;

/// Element-nesting depth beyond which the storage renderer stops tracking
/// frames (deeper tags become transparent).
const MAX_DEPTH: usize = 256;

/// ADF nesting depth beyond which the renderer stops descending.
const ADF_MAX_DEPTH: usize = 64;

/// Largest index `<= limit` that falls on a UTF-8 character boundary of `s`.
pub fn truncation_boundary(s: &str, limit: usize) -> usize {
    let mut index = limit.min(s.len());
    while !s.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// Cuts `text` to at most `max_chars` characters (not bytes), appending
/// [`TRUNCATED`] when anything was removed. Returns the text and whether it
/// was cut.
pub fn truncate_chars(text: &str, max_chars: usize) -> (String, bool) {
    match text.char_indices().nth(max_chars) {
        Some((byte_index, _)) => {
            let mut out = text[..byte_index].to_owned();
            out.push_str(TRUNCATED);
            (out, true)
        }
        None => (text.to_owned(), false),
    }
}

/// Whether `ch` may appear anywhere in an XML 1.0 document (text, attribute
/// or CDATA). Tab, LF and CR are the only allowed C0 controls; `U+007F` is
/// technically legal but is a control character with no rendering, so it is
/// excluded too. Surrogates cannot occur in a Rust `char`.
pub fn xml_allowed(ch: char) -> bool {
    !matches!(
        ch,
        '\u{0}'..='\u{8}' | '\u{B}' | '\u{C}' | '\u{E}'..='\u{1F}' | '\u{7F}' | '\u{FFFE}' | '\u{FFFF}'
    )
}

/// Removes every character [`xml_allowed`] rejects. Returns a borrowed view
/// when nothing had to be removed (the common case).
pub fn strip_xml_illegal(text: &str) -> std::borrow::Cow<'_, str> {
    if text.chars().all(xml_allowed) {
        std::borrow::Cow::Borrowed(text)
    } else {
        std::borrow::Cow::Owned(text.chars().filter(|ch| xml_allowed(*ch)).collect())
    }
}

/// Escapes text for an XML text node or attribute value, dropping characters
/// XML 1.0 forbids (see [`xml_allowed`]).
pub fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            ch if xml_allowed(ch) => out.push(ch),
            _ => {}
        }
    }
    out
}

/// Decodes the entities that appear in Confluence storage format: the five
/// XML ones, numeric references, and the handful of HTML names Confluence
/// emits (`&nbsp;` above all). Unknown names are left as written.
pub fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        let after = &rest[pos..];
        // An entity is at most ~10 chars; bound the scan so a lone `&` in a
        // long run of text does not cost a full search.
        let window_end = truncation_boundary(after, 12);
        let decoded = after[..window_end]
            .find(';')
            .and_then(|semi| entity_value(&after[1..semi]).map(|value| (value, semi + 1)));
        match decoded {
            Some((value, consumed)) => {
                out.push_str(&value);
                rest = &after[consumed..];
            }
            None => {
                out.push('&');
                rest = &after[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn entity_value(name: &str) -> Option<String> {
    let named = match name {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some(' '),
        "ndash" => Some('–'),
        "mdash" => Some('—'),
        "hellip" => Some('…'),
        "lsquo" => Some('‘'),
        "rsquo" => Some('’'),
        "ldquo" => Some('“'),
        "rdquo" => Some('”'),
        "copy" => Some('©'),
        "reg" => Some('®'),
        "trade" => Some('™'),
        "middot" => Some('·'),
        "bull" => Some('•'),
        "laquo" => Some('«'),
        "raquo" => Some('»'),
        "times" => Some('×'),
        "deg" => Some('°'),
        "euro" => Some('€'),
        "pound" => Some('£'),
        "yen" => Some('¥'),
        _ => None,
    };
    if let Some(ch) = named {
        return Some(ch.to_string());
    }
    let numeric = name.strip_prefix('#')?;
    let code = match numeric.strip_prefix(['x', 'X']) {
        Some(hex) => u32::from_str_radix(hex, 16).ok()?,
        None => numeric.parse::<u32>().ok()?,
    };
    char::from_u32(code)
        .filter(|ch| *ch != '\0')
        .map(|ch| ch.to_string())
}

// ---------------------------------------------------------------------------
// Storage-format tokenizer
// ---------------------------------------------------------------------------

/// One lexical piece of a storage-format document.
#[derive(Debug)]
enum Token<'a> {
    Text(&'a str),
    CData(&'a str),
    Open {
        name: &'a str,
        attrs: Vec<(&'a str, String)>,
        self_closing: bool,
    },
    Close(&'a str),
}

/// A tolerant, non-recursive XHTML tokenizer. It never fails: anything it
/// cannot make sense of is emitted as text.
fn tokenize(input: &str) -> Vec<Token<'_>> {
    let mut tokens = Vec::new();
    let mut rest = input;
    while !rest.is_empty() {
        let Some(lt) = rest.find('<') else {
            tokens.push(Token::Text(rest));
            break;
        };
        if lt > 0 {
            tokens.push(Token::Text(&rest[..lt]));
        }
        let tag = &rest[lt..];
        if let Some(after) = tag.strip_prefix("<!--") {
            rest = match after.find("-->") {
                Some(end) => &after[end + 3..],
                None => "",
            };
            continue;
        }
        if let Some(after) = tag.strip_prefix("<![CDATA[") {
            match after.find("]]>") {
                Some(end) => {
                    tokens.push(Token::CData(&after[..end]));
                    rest = &after[end + 3..];
                }
                None => {
                    tokens.push(Token::CData(after));
                    rest = "";
                }
            }
            continue;
        }
        if tag.starts_with("<!") || tag.starts_with("<?") {
            rest = match tag.find('>') {
                Some(end) => &tag[end + 1..],
                None => "",
            };
            continue;
        }
        if let Some(after) = tag.strip_prefix("</") {
            match after.find('>') {
                Some(end) => {
                    tokens.push(Token::Close(after[..end].trim()));
                    rest = &after[end + 1..];
                }
                None => {
                    tokens.push(Token::Text(tag));
                    rest = "";
                }
            }
            continue;
        }
        match parse_open_tag(tag) {
            Some((token, consumed)) => {
                tokens.push(token);
                rest = &tag[consumed..];
            }
            None => {
                // A bare `<` that is not a tag: keep it as text.
                tokens.push(Token::Text("<"));
                rest = &tag[1..];
            }
        }
    }
    tokens
}

/// Parses `<name attr="v" ... >` or `<name ... />` at the start of `tag`.
/// Returns the token and the number of bytes consumed.
fn parse_open_tag(tag: &str) -> Option<(Token<'_>, usize)> {
    let bytes = tag.as_bytes();
    let mut i = 1;
    let name_start = i;
    while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'>' && bytes[i] != b'/'
    {
        i += 1;
    }
    if i == name_start || !tag.is_char_boundary(i) {
        return None;
    }
    let name = &tag[name_start..i];
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'-' | b'_' | b'.'))
    {
        return None;
    }
    let mut attrs = Vec::new();
    loop {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            return None;
        }
        if bytes[i] == b'>' {
            return Some((
                Token::Open {
                    name,
                    attrs,
                    self_closing: false,
                },
                i + 1,
            ));
        }
        if bytes[i] == b'/' {
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b'>' {
                return Some((
                    Token::Open {
                        name,
                        attrs,
                        self_closing: true,
                    },
                    i + 1,
                ));
            }
            continue;
        }
        // Attribute name.
        let attr_start = i;
        while i < bytes.len()
            && !bytes[i].is_ascii_whitespace()
            && !matches!(bytes[i], b'=' | b'>' | b'/')
        {
            i += 1;
        }
        if i == attr_start {
            // Unexpected byte; skip it rather than loop forever.
            i += 1;
            continue;
        }
        if !tag.is_char_boundary(attr_start) || !tag.is_char_boundary(i) {
            return None;
        }
        let attr_name = &tag[attr_start..i];
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut value = String::new();
        if i < bytes.len() && bytes[i] == b'=' {
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\'') {
                let quote = bytes[i];
                i += 1;
                let value_start = i;
                while i < bytes.len() && bytes[i] != quote {
                    i += 1;
                }
                if !tag.is_char_boundary(i) {
                    return None;
                }
                value = decode_entities(&tag[value_start..i]);
                if i < bytes.len() {
                    i += 1;
                }
            } else {
                let value_start = i;
                while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'>' {
                    i += 1;
                }
                if !tag.is_char_boundary(i) {
                    return None;
                }
                value = decode_entities(&tag[value_start..i]);
            }
        }
        attrs.push((attr_name, value));
    }
}

// ---------------------------------------------------------------------------
// Storage-format → text renderer
// ---------------------------------------------------------------------------

/// What an open element contributes when it closes.
#[derive(Debug)]
enum Frame {
    /// Nothing special (span, div, unknown elements).
    Transparent,
    /// Element whose content is dropped (`ac:parameter`, `ac:placeholder`).
    Suppressed,
    Block,
    Heading,
    Inline(&'static str),
    Pre,
    List {
        ordered: bool,
        counter: usize,
    },
    Item,
    Link {
        href: String,
    },
    AcLink {
        target: String,
    },
    AcImage {
        source: String,
    },
    Macro {
        name: String,
        params: Vec<(String, String)>,
        plain_body: String,
    },
    MacroParam {
        name: String,
    },
    MacroPlainBody,
    Task {
        complete: bool,
    },
    TaskStatus,
    BlockQuote,
    Table {
        rows: Vec<Vec<String>>,
        header_rows: usize,
    },
    TableRow {
        cells: Vec<String>,
        all_header: bool,
    },
    TableCell {
        header: bool,
    },
}

struct StorageRenderer {
    /// Output buffers; nested elements that need their own text (links,
    /// cells, quotes) push one and pop it on close.
    outs: Vec<String>,
    frames: Vec<Frame>,
    /// Open tags beyond [`MAX_DEPTH`] that were not given a frame.
    overflow: usize,
    /// Nesting inside `<pre>`/code macros: whitespace is preserved.
    pre_depth: usize,
    /// Nesting inside suppressed frames: text is dropped.
    suppress_depth: usize,
    /// Total bytes emitted across all buffers; rendering stops at RENDER_CAP.
    emitted: usize,
    capped: bool,
}

impl StorageRenderer {
    fn new() -> Self {
        Self {
            outs: vec![String::new()],
            frames: Vec::new(),
            overflow: 0,
            pre_depth: 0,
            suppress_depth: 0,
            emitted: 0,
            capped: false,
        }
    }

    fn out(&mut self) -> &mut String {
        if self.outs.is_empty() {
            self.outs.push(String::new());
        }
        // `outs` is never empty after the guard above.
        self.outs.last_mut().expect("output stack is non-empty")
    }

    fn push(&mut self, text: &str) {
        if self.capped || text.is_empty() {
            return;
        }
        if self.emitted.saturating_add(text.len()) > RENDER_CAP {
            self.capped = true;
            return;
        }
        self.emitted += text.len();
        self.out().push_str(text);
    }

    fn ends_with_newline(&mut self) -> bool {
        let out = self.out();
        out.is_empty() || out.ends_with('\n')
    }

    fn ensure_newline(&mut self) {
        if !self.ends_with_newline() {
            self.push("\n");
        }
    }

    fn ensure_blank_line(&mut self) {
        self.ensure_newline();
        let out = self.out();
        if !out.is_empty() && !out.ends_with("\n\n") {
            self.push("\n");
        }
    }

    fn list_depth(&self) -> usize {
        self.frames
            .iter()
            .filter(|frame| matches!(frame, Frame::List { .. }))
            .count()
    }

    fn text(&mut self, raw: &str, cdata: bool) {
        if self.suppress_depth > 0 {
            // Captured by the innermost capturing frame instead.
            self.capture(raw, cdata);
            return;
        }
        let decoded = if cdata {
            raw.to_owned()
        } else {
            decode_entities(raw)
        };
        if self.pre_depth > 0 {
            self.push(&decoded);
            return;
        }
        let mut collapsed = String::with_capacity(decoded.len());
        let mut last_space = false;
        for ch in decoded.chars() {
            if ch.is_whitespace() {
                if !last_space {
                    collapsed.push(' ');
                    last_space = true;
                }
            } else {
                collapsed.push(ch);
                last_space = false;
            }
        }
        if collapsed.is_empty() {
            return;
        }
        if collapsed == " " {
            let at_break = {
                let out = self.out();
                out.is_empty() || out.ends_with('\n') || out.ends_with(' ')
            };
            if at_break {
                return;
            }
        } else if collapsed.starts_with(' ') && self.ends_with_newline() {
            collapsed.remove(0);
        }
        self.push(&collapsed);
    }

    /// Text inside a suppressed frame goes to the nearest capturing frame
    /// (a macro parameter, a code macro body, a task status).
    fn capture(&mut self, raw: &str, cdata: bool) {
        let decoded = if cdata {
            raw.to_owned()
        } else {
            decode_entities(raw)
        };
        let mut param: Option<String> = None;
        let mut plain = false;
        let mut status = false;
        for frame in self.frames.iter().rev() {
            match frame {
                Frame::MacroParam { name } => {
                    param = Some(name.clone());
                    break;
                }
                Frame::MacroPlainBody => {
                    plain = true;
                    break;
                }
                Frame::TaskStatus => {
                    status = true;
                    break;
                }
                Frame::Suppressed => return,
                _ => {}
            }
        }
        if let Some(name) = param {
            for frame in self.frames.iter_mut().rev() {
                if let Frame::Macro { params, .. } = frame {
                    if params.len() < 64 {
                        params.push((name, decoded));
                    }
                    return;
                }
            }
        } else if plain {
            for frame in self.frames.iter_mut().rev() {
                if let Frame::Macro { plain_body, .. } = frame {
                    if plain_body.len() < RENDER_CAP {
                        plain_body.push_str(&decoded);
                    }
                    return;
                }
            }
            // A plain-text body outside a macro: show it as code.
            self.push(&decoded);
        } else if status {
            let complete = decoded.trim().eq_ignore_ascii_case("complete");
            for frame in self.frames.iter_mut().rev() {
                if let Frame::Task { complete: slot } = frame {
                    *slot = complete;
                    return;
                }
            }
        }
    }

    fn open(&mut self, name: &str, attrs: &[(&str, String)], self_closing: bool) {
        let attr = |key: &str| -> String {
            attrs
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(key))
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        let lower = name.to_ascii_lowercase();
        // Self-closing elements that only contribute data to their parent.
        match lower.as_str() {
            "br" => {
                if self.suppress_depth == 0 {
                    self.push("\n");
                }
                return;
            }
            "hr" => {
                if self.suppress_depth == 0 {
                    self.ensure_blank_line();
                    self.push("---\n\n");
                }
                return;
            }
            "img" => {
                if self.suppress_depth == 0 {
                    let src = attr("src");
                    let alt = attr("alt");
                    let label = if alt.is_empty() {
                        src.as_str()
                    } else {
                        alt.as_str()
                    };
                    self.push(&format!("![{label}]"));
                }
                return;
            }
            "ri:page" | "ri:blog-post" => {
                let title = attr("ri:content-title");
                self.set_link_target(&title);
                return;
            }
            "ri:attachment" => {
                let filename = attr("ri:filename");
                self.set_link_target(&filename);
                return;
            }
            "ri:url" => {
                let url = attr("ri:value");
                self.set_link_target(&url);
                return;
            }
            "ri:user" => {
                let id = attr("ri:account-id");
                self.set_link_target(&format!("@{id}"));
                return;
            }
            "ri:space" => {
                let key = attr("ri:space-key");
                self.set_link_target(&key);
                return;
            }
            "ac:emoticon" => {
                if self.suppress_depth == 0 {
                    let fallback = attr("ac:emoji-fallback");
                    let text = if fallback.is_empty() {
                        format!(":{}:", attr("ac:name"))
                    } else {
                        fallback
                    };
                    self.push(&text);
                }
                return;
            }
            "time" => {
                if self.suppress_depth == 0 {
                    self.push(&attr("datetime"));
                }
                return;
            }
            _ => {}
        }
        if self_closing {
            // A bodiless macro (`<ac:structured-macro ac:name="toc" />`)
            // still deserves its marker.
            if matches!(lower.as_str(), "ac:structured-macro" | "ac:macro")
                && self.suppress_depth == 0
            {
                let name = attr("ac:name");
                if !name.is_empty() {
                    self.ensure_newline();
                    self.push(&format!("[macro:{name}]\n"));
                }
            }
            return;
        }
        if self.frames.len() >= MAX_DEPTH {
            self.overflow = self.overflow.saturating_add(1);
            return;
        }
        let frame = match lower.as_str() {
            "p" | "div" | "section" | "article" | "ac:layout" | "ac:layout-section"
            | "ac:layout-cell" | "ac:rich-text-body" | "ac:task-body" | "ac:adf-content"
            | "ac:adf-extension" | "ac:adf-node" | "tbody" | "thead" | "tfoot" | "colgroup"
            | "figure" => {
                if matches!(
                    lower.as_str(),
                    "p" | "div" | "section" | "article" | "figure"
                ) {
                    if self.suppress_depth == 0 {
                        self.ensure_newline();
                    }
                    Frame::Block
                } else {
                    Frame::Transparent
                }
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                let level = lower.as_bytes().get(1).map_or(1, |b| (b - b'0') as usize);
                if self.suppress_depth == 0 {
                    self.ensure_blank_line();
                    self.push(&"#".repeat(level.clamp(1, 6)));
                    self.push(" ");
                }
                Frame::Heading
            }
            "strong" | "b" => self.inline("**"),
            "em" | "i" => self.inline("_"),
            "code" | "tt" | "kbd" => self.inline("`"),
            "s" | "del" | "strike" => self.inline("~~"),
            "pre" => {
                if self.suppress_depth == 0 {
                    self.ensure_blank_line();
                    self.push("```\n");
                }
                self.pre_depth += 1;
                Frame::Pre
            }
            "ul" | "ac:task-list" => Frame::List {
                ordered: false,
                counter: 0,
            },
            "ol" => Frame::List {
                ordered: true,
                counter: attr("start")
                    .parse::<usize>()
                    .unwrap_or(1)
                    .saturating_sub(1),
            },
            "li" => {
                self.start_item(None);
                Frame::Item
            }
            "ac:task" => {
                self.outs.push(String::new());
                Frame::Task { complete: false }
            }
            "ac:task-status" => {
                self.suppress_depth += 1;
                Frame::TaskStatus
            }
            "ac:task-id"
            | "ac:parameter"
            | "ac:placeholder"
            | "ac:default-parameter"
            | "style"
            | "script"
            | "ri:page-metadata"
            | "ac:caption" => {
                self.suppress_depth += 1;
                if lower == "ac:parameter" {
                    Frame::MacroParam {
                        name: attr("ac:name"),
                    }
                } else {
                    Frame::Suppressed
                }
            }
            "ac:plain-text-body" => {
                self.suppress_depth += 1;
                Frame::MacroPlainBody
            }
            "a" => {
                self.outs.push(String::new());
                Frame::Link { href: attr("href") }
            }
            "ac:link" | "ac:link-body" | "ac:plain-text-link-body" => {
                if lower == "ac:link" {
                    self.outs.push(String::new());
                    Frame::AcLink {
                        target: String::new(),
                    }
                } else {
                    Frame::Transparent
                }
            }
            "ac:image" => {
                self.suppress_depth += 1;
                Frame::AcImage {
                    source: String::new(),
                }
            }
            "ac:structured-macro" | "ac:macro" => Frame::Macro {
                name: attr("ac:name"),
                params: Vec::new(),
                plain_body: String::new(),
            },
            "blockquote" => {
                if self.suppress_depth == 0 {
                    self.ensure_blank_line();
                }
                self.outs.push(String::new());
                Frame::BlockQuote
            }
            "table" => {
                if self.suppress_depth == 0 {
                    self.ensure_blank_line();
                }
                Frame::Table {
                    rows: Vec::new(),
                    header_rows: 0,
                }
            }
            "tr" => Frame::TableRow {
                cells: Vec::new(),
                all_header: true,
            },
            "th" | "td" => {
                self.outs.push(String::new());
                Frame::TableCell {
                    header: lower == "th",
                }
            }
            _ => Frame::Transparent,
        };
        // Macros other than code get a visible marker up front; the body text
        // (if any) follows naturally.
        if let Frame::Macro { name, .. } = &frame {
            if self.suppress_depth == 0
                && !matches!(
                    name.as_str(),
                    "code" | "noformat" | "" | "anchor" | "toc-zone"
                )
            {
                self.ensure_newline();
                self.push(&format!("[macro:{name}]"));
                self.push("\n");
            }
        }
        self.frames.push(frame);
    }

    fn inline(&mut self, marker: &'static str) -> Frame {
        if self.suppress_depth == 0 {
            self.push(marker);
        }
        Frame::Inline(marker)
    }

    fn set_link_target(&mut self, value: &str) {
        for frame in self.frames.iter_mut().rev() {
            match frame {
                Frame::AcLink { target } if target.is_empty() => {
                    *target = value.to_owned();
                    return;
                }
                Frame::AcImage { source } if source.is_empty() => {
                    *source = value.to_owned();
                    return;
                }
                _ => {}
            }
        }
    }

    fn start_item(&mut self, task: Option<bool>) {
        if self.suppress_depth > 0 {
            return;
        }
        self.ensure_newline();
        let depth = self.list_depth();
        let indent = "  ".repeat(depth.saturating_sub(1));
        let mut marker = String::from("- ");
        for frame in self.frames.iter_mut().rev() {
            if let Frame::List { ordered, counter } = frame {
                if *ordered {
                    *counter += 1;
                    marker = format!("{counter}. ");
                }
                break;
            }
        }
        self.push(&indent);
        self.push(&marker);
        if let Some(complete) = task {
            self.push(if complete { "[x] " } else { "[ ] " });
        }
    }

    fn close(&mut self, name: &str) {
        if self.overflow > 0 {
            self.overflow -= 1;
            return;
        }
        let lower = name.to_ascii_lowercase();
        let Some(index) = self
            .frames
            .iter()
            .rposition(|frame| frame_matches(frame, &lower))
        else {
            return;
        };
        while self.frames.len() > index {
            let Some(frame) = self.frames.pop() else {
                break;
            };
            self.close_frame(frame);
        }
    }

    fn pop_out(&mut self) -> String {
        if self.outs.len() > 1 {
            self.outs.pop().unwrap_or_default()
        } else {
            String::new()
        }
    }

    fn close_frame(&mut self, frame: Frame) {
        match frame {
            Frame::Transparent => {}
            Frame::Suppressed
            | Frame::MacroParam { .. }
            | Frame::MacroPlainBody
            | Frame::TaskStatus => {
                self.suppress_depth = self.suppress_depth.saturating_sub(1);
            }
            Frame::Block => {
                if self.suppress_depth == 0 {
                    self.ensure_blank_line();
                }
            }
            Frame::Heading => {
                if self.suppress_depth == 0 {
                    self.ensure_blank_line();
                }
            }
            Frame::Inline(marker) => {
                if self.suppress_depth == 0 {
                    self.push(marker);
                }
            }
            Frame::Pre => {
                self.pre_depth = self.pre_depth.saturating_sub(1);
                if self.suppress_depth == 0 {
                    self.ensure_newline();
                    self.push("```\n\n");
                }
            }
            Frame::List { .. } => {
                if self.suppress_depth == 0 {
                    self.ensure_newline();
                    if self.list_depth() == 0 {
                        self.push("\n");
                    }
                }
            }
            Frame::Item => {
                if self.suppress_depth == 0 {
                    self.ensure_newline();
                }
            }
            Frame::Task { complete } => {
                let body = self.pop_out();
                self.start_item(Some(complete));
                let body = body.trim().replace('\n', " ");
                self.push(&body);
                self.push("\n");
            }
            Frame::Link { href } => {
                let text = self.pop_out();
                let text = text.trim();
                if self.suppress_depth > 0 {
                    return;
                }
                let href = href.trim();
                if href.is_empty() || text == href {
                    self.push(if text.is_empty() { href } else { text });
                } else if text.is_empty() {
                    self.push(&format!("<{href}>"));
                } else {
                    self.push(&format!("[{text}]({href})"));
                }
            }
            Frame::AcLink { target } => {
                let text = self.pop_out();
                let text = text.trim();
                if self.suppress_depth > 0 {
                    return;
                }
                let rendered = match (text.is_empty(), target.is_empty()) {
                    (true, true) => String::new(),
                    (true, false) => format!("[{target}]"),
                    (false, true) => format!("[{text}]"),
                    (false, false) if text == target => format!("[{text}]"),
                    (false, false) => format!("[{text}]({target})"),
                };
                self.push(&rendered);
            }
            Frame::AcImage { source } => {
                self.suppress_depth = self.suppress_depth.saturating_sub(1);
                if self.suppress_depth == 0 {
                    self.push(&format!("![{source}]"));
                }
            }
            Frame::Macro {
                name,
                params,
                plain_body,
            } => {
                if self.suppress_depth > 0 {
                    return;
                }
                match name.as_str() {
                    "code" | "noformat" => {
                        let language = params
                            .iter()
                            .find(|(key, _)| key == "language")
                            .map(|(_, value)| value.trim().to_owned())
                            .unwrap_or_default();
                        self.ensure_blank_line();
                        self.push(&format!("```{language}\n"));
                        self.push(plain_body.trim_end_matches('\n'));
                        self.push("\n```\n\n");
                    }
                    _ => {
                        if !plain_body.trim().is_empty() {
                            self.ensure_blank_line();
                            self.push("```\n");
                            self.push(plain_body.trim_end_matches('\n'));
                            self.push("\n```\n\n");
                        }
                        self.ensure_newline();
                    }
                }
            }
            Frame::BlockQuote => {
                let inner = self.pop_out();
                if self.suppress_depth > 0 {
                    return;
                }
                let mut quoted = String::with_capacity(inner.len() + 16);
                for line in inner.trim().lines() {
                    quoted.push_str("> ");
                    quoted.push_str(line);
                    quoted.push('\n');
                }
                self.ensure_blank_line();
                self.push(&quoted);
                self.push("\n");
            }
            Frame::TableCell { header } => {
                let inner = self.pop_out();
                let cell = inner.trim().replace('\n', " ").replace('|', "\\|");
                for frame in self.frames.iter_mut().rev() {
                    if let Frame::TableRow { cells, all_header } = frame {
                        if cells.len() < 256 {
                            cells.push(cell);
                        }
                        if !header {
                            *all_header = false;
                        }
                        return;
                    }
                }
                // A cell outside a row: emit inline.
                self.push(&cell);
            }
            Frame::TableRow { cells, all_header } => {
                for frame in self.frames.iter_mut().rev() {
                    if let Frame::Table { rows, header_rows } = frame {
                        if all_header && !cells.is_empty() && *header_rows == rows.len() {
                            *header_rows += 1;
                        }
                        if rows.len() < 10_000 {
                            rows.push(cells);
                        }
                        return;
                    }
                }
            }
            Frame::Table { rows, header_rows } => {
                if self.suppress_depth > 0 || rows.is_empty() {
                    return;
                }
                let columns = rows.iter().map(Vec::len).max().unwrap_or(1).max(1);
                let mut text = String::new();
                let header_rows = header_rows.max(1).min(rows.len());
                for (index, row) in rows.iter().enumerate() {
                    text.push('|');
                    for column in 0..columns {
                        text.push(' ');
                        text.push_str(row.get(column).map_or("", String::as_str));
                        text.push_str(" |");
                    }
                    text.push('\n');
                    if index + 1 == header_rows {
                        text.push('|');
                        for _ in 0..columns {
                            text.push_str(" --- |");
                        }
                        text.push('\n');
                    }
                }
                self.ensure_blank_line();
                self.push(&text);
                self.push("\n");
            }
        }
    }

    fn finish(mut self) -> String {
        while let Some(frame) = self.frames.pop() {
            self.close_frame(frame);
        }
        while self.outs.len() > 1 {
            let inner = self.pop_out();
            self.push(&inner);
        }
        let raw = self.outs.pop().unwrap_or_default();
        let mut cleaned = String::with_capacity(raw.len());
        let mut blank_run = 0usize;
        for line in raw.lines() {
            let line = line.trim_end();
            if line.is_empty() {
                blank_run += 1;
                if blank_run > 1 {
                    continue;
                }
            } else {
                blank_run = 0;
            }
            cleaned.push_str(line);
            cleaned.push('\n');
        }
        let mut cleaned = cleaned.trim().to_owned();
        if self.capped {
            cleaned.push_str(TRUNCATED);
        }
        cleaned
    }
}

fn frame_matches(frame: &Frame, lower: &str) -> bool {
    match frame {
        // Any name that `open` does not give a specific frame becomes a
        // Transparent frame, so a Transparent frame matches exactly those.
        Frame::Transparent => !has_specific_frame(lower),
        Frame::Suppressed => matches!(
            lower,
            "ac:task-id"
                | "ac:placeholder"
                | "ac:default-parameter"
                | "style"
                | "script"
                | "ri:page-metadata"
                | "ac:caption"
        ),
        Frame::Block => matches!(lower, "p" | "div" | "section" | "article" | "figure"),
        Frame::Heading => matches!(lower, "h1" | "h2" | "h3" | "h4" | "h5" | "h6"),
        Frame::Inline("**") => matches!(lower, "strong" | "b"),
        Frame::Inline("_") => matches!(lower, "em" | "i"),
        Frame::Inline("`") => matches!(lower, "code" | "tt" | "kbd"),
        Frame::Inline("~~") => matches!(lower, "s" | "del" | "strike"),
        Frame::Inline(_) => false,
        Frame::Pre => lower == "pre",
        Frame::List { ordered: true, .. } => lower == "ol",
        Frame::List { ordered: false, .. } => matches!(lower, "ul" | "ac:task-list"),
        Frame::Item => lower == "li",
        Frame::Link { .. } => lower == "a",
        Frame::AcLink { .. } => lower == "ac:link",
        Frame::AcImage { .. } => lower == "ac:image",
        Frame::Macro { .. } => matches!(lower, "ac:structured-macro" | "ac:macro"),
        Frame::MacroParam { .. } => lower == "ac:parameter",
        Frame::MacroPlainBody => lower == "ac:plain-text-body",
        Frame::Task { .. } => lower == "ac:task",
        Frame::TaskStatus => lower == "ac:task-status",
        Frame::BlockQuote => lower == "blockquote",
        Frame::Table { .. } => lower == "table",
        Frame::TableRow { .. } => lower == "tr",
        Frame::TableCell { .. } => matches!(lower, "th" | "td"),
    }
}

/// Element names for which [`StorageRenderer::open`] creates a frame other
/// than [`Frame::Transparent`]. Must stay in step with that `match`.
fn has_specific_frame(lower: &str) -> bool {
    matches!(
        lower,
        "p" | "div"
            | "section"
            | "article"
            | "figure"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "strong"
            | "b"
            | "em"
            | "i"
            | "code"
            | "tt"
            | "kbd"
            | "s"
            | "del"
            | "strike"
            | "pre"
            | "ul"
            | "ol"
            | "li"
            | "a"
            | "ac:link"
            | "ac:image"
            | "ac:structured-macro"
            | "ac:macro"
            | "ac:parameter"
            | "ac:plain-text-body"
            | "ac:task-list"
            | "ac:task"
            | "ac:task-status"
            | "ac:task-id"
            | "ac:placeholder"
            | "ac:default-parameter"
            | "ac:caption"
            | "ri:page-metadata"
            | "style"
            | "script"
            | "blockquote"
            | "table"
            | "tr"
            | "th"
            | "td"
    )
}

/// Renders Confluence storage-format XHTML (or the `view` HTML) to readable
/// text. Lossy by design: macros become `[macro:name]` markers followed by
/// their rich-text body, code macros become fenced blocks, images become
/// `![filename]`, links keep their target.
pub fn storage_to_text(storage: &str) -> String {
    let mut renderer = StorageRenderer::new();
    for token in tokenize(storage) {
        match token {
            Token::Text(text) => renderer.text(text, false),
            Token::CData(text) => renderer.text(text, true),
            Token::Open {
                name,
                attrs,
                self_closing,
            } => renderer.open(name, &attrs, self_closing),
            Token::Close(name) => renderer.close(name),
        }
        if renderer.capped {
            break;
        }
    }
    renderer.finish()
}

// ---------------------------------------------------------------------------
// ADF → text
// ---------------------------------------------------------------------------

struct AdfRenderer {
    out: String,
    capped: bool,
}

impl AdfRenderer {
    fn push(&mut self, text: &str) {
        if self.capped {
            return;
        }
        if self.out.len().saturating_add(text.len()) > RENDER_CAP {
            self.capped = true;
            return;
        }
        self.out.push_str(text);
    }

    fn ensure_newline(&mut self) {
        if !(self.out.is_empty() || self.out.ends_with('\n')) {
            self.push("\n");
        }
    }

    fn ensure_blank_line(&mut self) {
        self.ensure_newline();
        if !self.out.is_empty() && !self.out.ends_with("\n\n") {
            self.push("\n");
        }
    }

    fn children(&mut self, node: &serde_json::Value, depth: usize, list_depth: usize) {
        if let Some(content) = node.get("content").and_then(|c| c.as_array()) {
            for child in content {
                if self.capped {
                    return;
                }
                self.node(child, depth + 1, list_depth);
            }
        }
    }

    fn inline_children(&mut self, node: &serde_json::Value, depth: usize) -> String {
        let mut inner = AdfRenderer {
            out: String::new(),
            capped: false,
        };
        inner.children(node, depth, 0);
        if inner.capped {
            self.capped = true;
        }
        inner.out
    }

    fn node(&mut self, node: &serde_json::Value, depth: usize, list_depth: usize) {
        if depth > ADF_MAX_DEPTH || self.capped {
            return;
        }
        let kind = node.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let attrs = node.get("attrs");
        let attr = |key: &str| -> String {
            attrs
                .and_then(|a| a.get(key))
                .map(|v| match v {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .unwrap_or_default()
        };
        match kind {
            "doc" => self.children(node, depth, list_depth),
            "paragraph" => {
                self.children(node, depth, list_depth);
                if list_depth == 0 {
                    self.ensure_blank_line();
                } else {
                    self.ensure_newline();
                }
            }
            "heading" => {
                let level = attrs
                    .and_then(|a| a.get("level"))
                    .and_then(|l| l.as_u64())
                    .unwrap_or(1)
                    .clamp(1, 6) as usize;
                self.ensure_blank_line();
                self.push(&"#".repeat(level));
                self.push(" ");
                self.children(node, depth, list_depth);
                self.ensure_blank_line();
            }
            "text" => {
                let text = node.get("text").and_then(|t| t.as_str()).unwrap_or("");
                let mut prefix = String::new();
                let mut suffix = String::new();
                let mut href: Option<String> = None;
                if let Some(marks) = node.get("marks").and_then(|m| m.as_array()) {
                    for mark in marks {
                        match mark.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                            "strong" => {
                                prefix.push_str("**");
                                suffix.insert_str(0, "**");
                            }
                            "em" => {
                                prefix.push('_');
                                suffix.insert(0, '_');
                            }
                            "code" => {
                                prefix.push('`');
                                suffix.insert(0, '`');
                            }
                            "strike" => {
                                prefix.push_str("~~");
                                suffix.insert_str(0, "~~");
                            }
                            "link" => {
                                href = mark
                                    .get("attrs")
                                    .and_then(|a| a.get("href"))
                                    .and_then(|h| h.as_str())
                                    .map(str::to_owned);
                            }
                            _ => {}
                        }
                    }
                }
                match href {
                    Some(href) if href != text => {
                        self.push(&format!("{prefix}[{text}]({href}){suffix}"));
                    }
                    _ => {
                        self.push(&prefix);
                        self.push(text);
                        self.push(&suffix);
                    }
                }
            }
            "hardBreak" => self.push("\n"),
            "rule" => {
                self.ensure_blank_line();
                self.push("---\n\n");
            }
            "bulletList" | "orderedList" | "taskList" | "decisionList" => {
                let ordered = kind == "orderedList";
                let start = attrs
                    .and_then(|a| a.get("order"))
                    .and_then(|o| o.as_u64())
                    .unwrap_or(1);
                let mut counter = start.saturating_sub(1);
                if let Some(items) = node.get("content").and_then(|c| c.as_array()) {
                    for item in items {
                        if self.capped {
                            return;
                        }
                        counter = counter.saturating_add(1);
                        let item_kind = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        self.ensure_newline();
                        self.push(&"  ".repeat(list_depth));
                        if ordered {
                            self.push(&format!("{counter}. "));
                        } else {
                            self.push("- ");
                        }
                        if item_kind == "taskItem" {
                            let done = item
                                .get("attrs")
                                .and_then(|a| a.get("state"))
                                .and_then(|s| s.as_str())
                                .is_some_and(|s| s.eq_ignore_ascii_case("DONE"));
                            self.push(if done { "[x] " } else { "[ ] " });
                        } else if item_kind == "decisionItem" {
                            self.push("(decision) ");
                        }
                        self.children(item, depth + 1, list_depth + 1);
                        self.ensure_newline();
                    }
                }
                if list_depth == 0 {
                    self.ensure_blank_line();
                }
            }
            "listItem" | "taskItem" | "decisionItem" => {
                self.children(node, depth, list_depth.max(1));
            }
            "codeBlock" => {
                let language = attr("language");
                self.ensure_blank_line();
                self.push(&format!("```{language}\n"));
                let text = self.inline_children(node, depth);
                self.push(text.trim_end_matches('\n'));
                self.push("\n```\n\n");
            }
            "blockquote" => {
                let inner = self.inline_children(node, depth);
                self.ensure_blank_line();
                for line in inner.trim().lines() {
                    self.push("> ");
                    self.push(line);
                    self.push("\n");
                }
                self.push("\n");
            }
            "panel" => {
                let panel_type = attr("panelType");
                self.ensure_blank_line();
                self.push(&format!("[panel:{panel_type}]\n"));
                self.children(node, depth, list_depth);
                self.ensure_blank_line();
            }
            "expand" | "nestedExpand" => {
                let title = attr("title");
                self.ensure_blank_line();
                self.push(&format!("[expand: {title}]\n"));
                self.children(node, depth, list_depth);
                self.ensure_blank_line();
            }
            "table" => {
                let mut rows: Vec<Vec<String>> = Vec::new();
                let mut header_rows = 0usize;
                if let Some(content) = node.get("content").and_then(|c| c.as_array()) {
                    for row in content.iter().take(10_000) {
                        let mut cells = Vec::new();
                        let mut all_header = true;
                        if let Some(row_content) = row.get("content").and_then(|c| c.as_array()) {
                            for cell in row_content.iter().take(256) {
                                let is_header = cell.get("type").and_then(|t| t.as_str())
                                    == Some("tableHeader");
                                if !is_header {
                                    all_header = false;
                                }
                                let text = self.inline_children(cell, depth + 2);
                                cells.push(text.trim().replace('\n', " ").replace('|', "\\|"));
                            }
                        }
                        if all_header && !cells.is_empty() && header_rows == rows.len() {
                            header_rows += 1;
                        }
                        rows.push(cells);
                    }
                }
                if rows.is_empty() {
                    return;
                }
                let columns = rows.iter().map(Vec::len).max().unwrap_or(1).max(1);
                let header_rows = header_rows.max(1).min(rows.len());
                self.ensure_blank_line();
                for (index, row) in rows.iter().enumerate() {
                    self.push("|");
                    for column in 0..columns {
                        self.push(" ");
                        self.push(row.get(column).map_or("", String::as_str));
                        self.push(" |");
                    }
                    self.push("\n");
                    if index + 1 == header_rows {
                        self.push("|");
                        for _ in 0..columns {
                            self.push(" --- |");
                        }
                        self.push("\n");
                    }
                }
                self.push("\n");
            }
            "mention" => {
                let text = attr("text");
                if text.is_empty() {
                    self.push(&format!("@{}", attr("id")));
                } else {
                    self.push(&text);
                }
            }
            "emoji" => {
                let text = attr("text");
                if text.is_empty() {
                    self.push(&attr("shortName"));
                } else {
                    self.push(&text);
                }
            }
            "inlineCard" | "blockCard" | "embedCard" => {
                let url = attr("url");
                if !url.is_empty() {
                    self.push(&format!("<{url}>"));
                }
                if kind != "inlineCard" {
                    self.ensure_blank_line();
                }
            }
            "date" => {
                let timestamp = attrs
                    .and_then(|a| a.get("timestamp"))
                    .and_then(|t| match t {
                        serde_json::Value::String(s) => s.parse::<i64>().ok(),
                        other => other.as_i64(),
                    });
                match timestamp {
                    Some(ms) => self.push(&format_date(ms)),
                    None => self.push("[date]"),
                }
            }
            "status" => self.push(&format!("[{}]", attr("text"))),
            "media" => {
                let alt = attr("alt");
                let url = attr("url");
                let id = attr("id");
                let label = if !alt.is_empty() {
                    alt
                } else if !url.is_empty() {
                    url
                } else {
                    id
                };
                self.push(&format!("![{label}]"));
            }
            "mediaSingle" | "mediaGroup" | "mediaInline" => {
                self.children(node, depth, list_depth);
                if kind != "mediaInline" {
                    self.ensure_blank_line();
                }
            }
            "extension" | "bodiedExtension" | "inlineExtension" => {
                let key = attr("extensionKey");
                self.push(&format!("[extension:{key}]"));
                self.children(node, depth, list_depth);
                if kind != "inlineExtension" {
                    self.ensure_blank_line();
                }
            }
            "placeholder" => self.push(&attr("text")),
            _ => self.children(node, depth, list_depth),
        }
    }
}

/// Formats a Unix-millisecond timestamp as `YYYY-MM-DD` (UTC).
fn format_date(millis: i64) -> String {
    let days = millis.div_euclid(86_400_000);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// Renders an ADF document (`{"type":"doc","content":[...]}`) to text.
pub fn adf_to_text(doc: &serde_json::Value) -> String {
    let mut renderer = AdfRenderer {
        out: String::new(),
        capped: false,
    };
    renderer.node(doc, 0, 0);
    let mut text = renderer.out.trim().to_owned();
    if renderer.capped {
        text.push_str(TRUNCATED);
    }
    text
}

// ---------------------------------------------------------------------------
// Markdown → storage format
// ---------------------------------------------------------------------------

/// Maps common fence-info aliases to the names Confluence's code macro knows.
fn code_language(info: &str) -> String {
    let first = info
        .split(|c: char| c.is_whitespace() || c == ',')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let mapped = match first.as_str() {
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "ts" | "tsx" => "typescript",
        "sh" | "shell" | "zsh" | "console" | "shell-session" => "bash",
        "py" | "python3" => "python",
        "rb" => "ruby",
        "yml" => "yaml",
        "c++" | "cc" | "cxx" | "hpp" => "cpp",
        "cs" => "csharp",
        "kt" | "kts" => "kotlin",
        "rs" => "rust",
        "ps1" | "pwsh" => "powershell",
        "md" => "markdown",
        "htm" => "html",
        "text" | "txt" | "plain" | "plaintext" => "none",
        other => other,
    };
    mapped
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '+' | '#'))
        .take(32)
        .collect()
}

/// Whether a link destination is safe to keep as an `href` (http(s), mailto,
/// or a relative/anchor path). Anything else is rendered as plain text.
fn safe_href(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return false;
    }
    if let Some(colon) = lower.find(':') {
        let scheme = &lower[..colon];
        if scheme
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
        {
            return matches!(scheme, "http" | "https" | "mailto");
        }
    }
    true
}

/// Splits a CDATA payload so a literal `]]>` inside code cannot terminate
/// the section early, and drops the control characters XML forbids even
/// inside CDATA (see [`xml_allowed`]).
fn cdata(text: &str) -> String {
    strip_xml_illegal(text).replace("]]>", "]]]]><![CDATA[>")
}

/// Converts Markdown (CommonMark + GFM tables, strikethrough, task lists)
/// to Confluence storage format. Raw HTML in the input is escaped as text —
/// storage format must stay well-formed XML and Confluence's own sanitizer
/// would strip most of it anyway.
pub fn markdown_to_storage(markdown: &str) -> String {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_FOOTNOTES);
    let events: Vec<Event<'_>> = Parser::new_ext(markdown, options).collect();

    let mut out = String::with_capacity(markdown.len() + markdown.len() / 4 + 64);
    // Stack of open lists: `true` when rendered as an ac:task-list.
    let mut lists: Vec<bool> = Vec::new();
    let mut task_counter = 0usize;
    let mut in_table_head = false;
    let mut in_code_block = false;
    let mut code_language_buf: Option<String> = None;
    let mut code_buf = String::new();
    // While collecting an image's alt text, the plain text goes here.
    let mut image: Option<(String, String)> = None;
    let mut skip_depth = 0usize;
    // Open links: `true` when the destination was unsafe and a `<span>` was
    // written instead of an `<a>`.
    let mut links: Vec<bool> = Vec::new();

    let mut index = 0usize;
    while index < events.len() {
        let event = &events[index];
        index += 1;
        if out.len() > RENDER_CAP {
            break;
        }
        // Inside an image, only the alt text matters.
        if let Some((_, alt)) = image.as_mut() {
            match event {
                Event::End(TagEnd::Image) => {
                    let (url, alt) = image.take().unwrap_or_default();
                    let alt_attr = xml_escape(alt.trim());
                    let url = url.trim();
                    let is_url = url.contains("://");
                    if is_url && safe_href(url) {
                        let _ = write!(
                            out,
                            "<ac:image ac:alt=\"{alt_attr}\"><ri:url ri:value=\"{}\" /></ac:image>",
                            xml_escape(url)
                        );
                    } else if !url.is_empty() && !url.contains('/') && !url.contains(':') {
                        let _ = write!(
                            out,
                            "<ac:image ac:alt=\"{alt_attr}\"><ri:attachment ri:filename=\"{}\" /></ac:image>",
                            xml_escape(url)
                        );
                    } else {
                        out.push_str(&alt_attr);
                    }
                }
                Event::Text(text) | Event::Code(text) => alt.push_str(text),
                _ => {}
            }
            continue;
        }
        if skip_depth > 0 {
            match event {
                Event::Start(_) => skip_depth += 1,
                Event::End(_) => skip_depth -= 1,
                _ => {}
            }
            continue;
        }
        if in_code_block {
            match event {
                Event::End(TagEnd::CodeBlock) => {
                    in_code_block = false;
                    let language = code_language_buf.take().unwrap_or_default();
                    out.push_str("<ac:structured-macro ac:name=\"code\">");
                    if !language.is_empty() && language != "none" {
                        let _ = write!(
                            out,
                            "<ac:parameter ac:name=\"language\">{}</ac:parameter>",
                            xml_escape(&language)
                        );
                    }
                    let _ = write!(
                        out,
                        "<ac:plain-text-body><![CDATA[{}]]></ac:plain-text-body></ac:structured-macro>",
                        cdata(code_buf.trim_end_matches('\n'))
                    );
                    code_buf.clear();
                }
                Event::Text(text) => code_buf.push_str(text),
                _ => {}
            }
            continue;
        }
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => out.push_str("<p>"),
                Tag::Heading { level, .. } => {
                    let _ = write!(out, "<h{}>", heading_number(*level));
                }
                Tag::BlockQuote(_) => out.push_str("<blockquote>"),
                Tag::CodeBlock(kind) => {
                    in_code_block = true;
                    code_language_buf = Some(match kind {
                        CodeBlockKind::Fenced(info) => code_language(info),
                        CodeBlockKind::Indented => String::new(),
                    });
                }
                Tag::HtmlBlock => out.push_str("<p>"),
                Tag::List(start) => {
                    // A list whose first item starts with a task marker is a
                    // Confluence task list.
                    let is_task = matches!(
                        (events.get(index), events.get(index + 1)),
                        (
                            Some(Event::Start(Tag::Item)),
                            Some(Event::TaskListMarker(_))
                        )
                    );
                    lists.push(is_task);
                    if is_task {
                        out.push_str("<ac:task-list>");
                    } else {
                        match start {
                            Some(n) if *n != 1 => {
                                let _ = write!(out, "<ol start=\"{n}\">");
                            }
                            Some(_) => out.push_str("<ol>"),
                            None => out.push_str("<ul>"),
                        }
                    }
                }
                Tag::Item => {
                    if lists.last().copied().unwrap_or(false) {
                        task_counter += 1;
                        let checked =
                            matches!(events.get(index), Some(Event::TaskListMarker(true)));
                        let _ = write!(
                            out,
                            "<ac:task><ac:task-id>{task_counter}</ac:task-id><ac:task-status>{}</ac:task-status><ac:task-body>",
                            if checked { "complete" } else { "incomplete" }
                        );
                    } else {
                        out.push_str("<li>");
                    }
                }
                Tag::FootnoteDefinition(label) => {
                    let _ = write!(out, "<p><sup>{}</sup> ", xml_escape(label));
                }
                Tag::DefinitionList | Tag::DefinitionListTitle | Tag::DefinitionListDefinition => {
                    out.push_str("<p>");
                }
                Tag::Table(_) => out.push_str("<table><tbody>"),
                Tag::TableHead => {
                    in_table_head = true;
                    out.push_str("<tr>");
                }
                Tag::TableRow => out.push_str("<tr>"),
                Tag::TableCell => out.push_str(if in_table_head { "<th>" } else { "<td>" }),
                Tag::Emphasis => out.push_str("<em>"),
                Tag::Strong => out.push_str("<strong>"),
                Tag::Strikethrough => out.push_str("<s>"),
                Tag::Superscript => out.push_str("<sup>"),
                Tag::Subscript => out.push_str("<sub>"),
                Tag::Link {
                    dest_url, title, ..
                } => {
                    let unsafe_link = !safe_href(dest_url);
                    links.push(unsafe_link);
                    if unsafe_link {
                        out.push_str("<span>");
                    } else {
                        let _ = write!(out, "<a href=\"{}\"", xml_escape(dest_url));
                        if !title.is_empty() {
                            let _ = write!(out, " title=\"{}\"", xml_escape(title));
                        }
                        out.push('>');
                    }
                }
                Tag::Image { dest_url, .. } => {
                    image = Some((dest_url.to_string(), String::new()));
                }
                Tag::MetadataBlock(_) => skip_depth = 1,
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => out.push_str("</p>"),
                TagEnd::Heading(level) => {
                    let _ = write!(out, "</h{}>", heading_number(*level));
                }
                TagEnd::BlockQuote(_) => out.push_str("</blockquote>"),
                TagEnd::CodeBlock => {}
                TagEnd::HtmlBlock => out.push_str("</p>"),
                TagEnd::List(ordered) => match lists.pop() {
                    Some(true) => out.push_str("</ac:task-list>"),
                    _ => out.push_str(if *ordered { "</ol>" } else { "</ul>" }),
                },
                TagEnd::Item => {
                    if lists.last().copied().unwrap_or(false) {
                        out.push_str("</ac:task-body></ac:task>");
                    } else {
                        out.push_str("</li>");
                    }
                }
                TagEnd::FootnoteDefinition => out.push_str("</p>"),
                TagEnd::DefinitionList
                | TagEnd::DefinitionListTitle
                | TagEnd::DefinitionListDefinition => out.push_str("</p>"),
                TagEnd::Table => out.push_str("</tbody></table>"),
                TagEnd::TableHead => {
                    in_table_head = false;
                    out.push_str("</tr>");
                }
                TagEnd::TableRow => out.push_str("</tr>"),
                TagEnd::TableCell => out.push_str(if in_table_head { "</th>" } else { "</td>" }),
                TagEnd::Emphasis => out.push_str("</em>"),
                TagEnd::Strong => out.push_str("</strong>"),
                TagEnd::Strikethrough => out.push_str("</s>"),
                TagEnd::Superscript => out.push_str("</sup>"),
                TagEnd::Subscript => out.push_str("</sub>"),
                TagEnd::Link => match links.pop() {
                    Some(true) => out.push_str("</span>"),
                    _ => out.push_str("</a>"),
                },
                TagEnd::Image => {}
                TagEnd::MetadataBlock(_) => {}
            },
            Event::Text(text) => out.push_str(&xml_escape(text)),
            Event::Code(text) => {
                let _ = write!(out, "<code>{}</code>", xml_escape(text));
            }
            Event::InlineMath(text) | Event::DisplayMath(text) => {
                let _ = write!(out, "<code>{}</code>", xml_escape(text));
            }
            Event::Html(text) | Event::InlineHtml(text) => out.push_str(&xml_escape(text)),
            Event::FootnoteReference(label) => {
                let _ = write!(out, "<sup>[{}]</sup>", xml_escape(label));
            }
            Event::SoftBreak => out.push(' '),
            Event::HardBreak => out.push_str("<br />"),
            Event::Rule => out.push_str("<hr />"),
            Event::TaskListMarker(_) => {}
        }
    }
    out
}

fn heading_number(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}
