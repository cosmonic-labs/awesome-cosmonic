//! A small, flat Markdown subset converted locally into Notion block objects
//! for `append_blocks`.
//!
//! Supported, one block per construct, no nesting:
//!
//! | Markdown | Block |
//! |---|---|
//! | `# `, `## `, `### ` | `heading_1` / `heading_2` / `heading_3` |
//! | `- ` or `* ` | `bulleted_list_item` |
//! | `1. ` (any number) | `numbered_list_item` |
//! | `- [ ] ` / `- [x] ` | `to_do` (unchecked / checked) |
//! | ```` ```lang ```` fenced block | `code` (language defaults to `plain text`) |
//! | `> ` | `quote` (consecutive lines join) |
//! | `---` / `***` | `divider` |
//! | anything else | `paragraph` (consecutive lines join; blank line separates) |
//!
//! Inline formatting is kept as plain text: the point of this converter is
//! predictable structure, not fidelity. Whole-page edits should go through
//! the Markdown endpoint (`update_page_markdown`), where Notion parses its
//! own dialect.

use serde_json::{json, Value};

use crate::notion::{rich_text_chunks, MAX_ARRAY_ITEMS};

/// Most Markdown characters accepted per call (Notion's payload cap is
/// 500 KB; 100 blocks of 2000-char runs are far below this).
pub const MAX_MARKDOWN_CHARS: usize = 200_000;

/// Converts Markdown into at most [`MAX_ARRAY_ITEMS`] blocks, or returns a
/// caller-facing error naming the limit that was hit.
pub fn to_blocks(markdown: &str) -> Result<Vec<Value>, String> {
    if markdown.trim().is_empty() {
        return Err("markdown is empty; nothing to append".to_owned());
    }
    if markdown.chars().count() > MAX_MARKDOWN_CHARS {
        return Err(format!(
            "markdown is longer than {MAX_MARKDOWN_CHARS} characters; split it into several \
             append_blocks calls"
        ));
    }

    let mut blocks: Vec<Value> = Vec::new();
    let mut paragraph: Vec<&str> = Vec::new();
    let mut quote: Vec<&str> = Vec::new();
    let mut code: Option<(String, Vec<&str>)> = None;

    fn flush_paragraph(lines: &mut Vec<&str>, blocks: &mut Vec<Value>) {
        if lines.is_empty() {
            return;
        }
        let text = lines.join("\n");
        lines.clear();
        blocks.push(text_block("paragraph", &text));
    }
    fn flush_quote(lines: &mut Vec<&str>, blocks: &mut Vec<Value>) {
        if lines.is_empty() {
            return;
        }
        let text = lines.join("\n");
        lines.clear();
        blocks.push(text_block("quote", &text));
    }

    for raw_line in markdown.lines() {
        let line = raw_line.trim_end();

        // Inside a fenced code block everything is literal until the fence.
        if let Some((language, lines)) = code.as_mut() {
            if line.trim_start().starts_with("```") {
                let body = lines.join("\n");
                let language = std::mem::take(language);
                code = None;
                blocks.push(json!({
                    "object": "block",
                    "type": "code",
                    "code": {
                        "rich_text": rich_text_chunks(&body),
                        "language": language,
                    }
                }));
            } else {
                lines.push(raw_line);
            }
            continue;
        }

        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("```") {
            flush_paragraph(&mut paragraph, &mut blocks);
            flush_quote(&mut quote, &mut blocks);
            let language = rest.trim();
            let language = if language.is_empty() {
                "plain text".to_owned()
            } else {
                language.to_ascii_lowercase()
            };
            code = Some((language, Vec::new()));
            continue;
        }

        if trimmed.is_empty() {
            flush_paragraph(&mut paragraph, &mut blocks);
            flush_quote(&mut quote, &mut blocks);
            continue;
        }

        if let Some(rest) = trimmed
            .strip_prefix("> ")
            .or_else(|| trimmed.strip_prefix('>'))
        {
            flush_paragraph(&mut paragraph, &mut blocks);
            quote.push(rest.trim_start());
            continue;
        }
        flush_quote(&mut quote, &mut blocks);

        if is_divider(trimmed) {
            flush_paragraph(&mut paragraph, &mut blocks);
            blocks.push(json!({ "object": "block", "type": "divider", "divider": {} }));
            continue;
        }

        if let Some(block) = heading(trimmed)
            .or_else(|| to_do(trimmed))
            .or_else(|| bullet(trimmed))
            .or_else(|| numbered(trimmed))
        {
            flush_paragraph(&mut paragraph, &mut blocks);
            blocks.push(block);
            continue;
        }

        paragraph.push(trimmed);
    }

    // An unterminated fence still becomes a code block.
    if let Some((language, lines)) = code.take() {
        let body = lines.join("\n");
        blocks.push(json!({
            "object": "block",
            "type": "code",
            "code": { "rich_text": rich_text_chunks(&body), "language": language }
        }));
    }
    flush_paragraph(&mut paragraph, &mut blocks);
    flush_quote(&mut quote, &mut blocks);

    if blocks.is_empty() {
        return Err("markdown produced no blocks; nothing to append".to_owned());
    }
    if blocks.len() > MAX_ARRAY_ITEMS {
        return Err(format!(
            "markdown converts to {} blocks; Notion accepts at most {MAX_ARRAY_ITEMS} per call \
             — split into multiple append_blocks calls",
            blocks.len()
        ));
    }
    Ok(blocks)
}

fn text_block(ty: &str, text: &str) -> Value {
    json!({
        "object": "block",
        "type": ty,
        ty: { "rich_text": rich_text_chunks(text) }
    })
}

fn is_divider(line: &str) -> bool {
    let compact: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    compact.len() >= 3
        && (compact.chars().all(|c| c == '-')
            || compact.chars().all(|c| c == '*')
            || compact.chars().all(|c| c == '_'))
}

fn heading(line: &str) -> Option<Value> {
    let level = line.chars().take_while(|c| *c == '#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let rest = line[level..].strip_prefix(' ')?;
    // Notion has three heading levels; deeper Markdown headings clamp to 3.
    let ty = match level {
        1 => "heading_1",
        2 => "heading_2",
        _ => "heading_3",
    };
    Some(text_block(ty, rest.trim()))
}

fn to_do(line: &str) -> Option<Value> {
    let rest = line
        .strip_prefix("- ")
        .or_else(|| line.strip_prefix("* "))?
        .trim_start();
    let (checked, text) = if let Some(t) = rest.strip_prefix("[ ]") {
        (false, t)
    } else if let Some(t) = rest
        .strip_prefix("[x]")
        .or_else(|| rest.strip_prefix("[X]"))
    {
        (true, t)
    } else {
        return None;
    };
    Some(json!({
        "object": "block",
        "type": "to_do",
        "to_do": { "rich_text": rich_text_chunks(text.trim()), "checked": checked }
    }))
}

fn bullet(line: &str) -> Option<Value> {
    let rest = line
        .strip_prefix("- ")
        .or_else(|| line.strip_prefix("* "))?;
    Some(text_block("bulleted_list_item", rest.trim()))
}

fn numbered(line: &str) -> Option<Value> {
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 || digits > 9 {
        return None;
    }
    let rest = line[digits..]
        .strip_prefix(". ")
        .or_else(|| line[digits..].strip_prefix(") "))?;
    Some(text_block("numbered_list_item", rest.trim()))
}
