//! CSV Tidy — paste a messy CSV, get clean JSON and a list of what was wrong.
//!
//! The kind of file that arrives from a bank export, a CRM, or a spreadsheet
//! someone edited by hand: a BOM at the front, a stray blank line, a header row
//! with two columns called "Notes", and one row with an extra comma in it. This
//! reports each of those rather than silently coping, because on a real export
//! the ragged row is usually the interesting one.
//!
//! It declares no outbound network access. That is the point for this
//! particular tool: the files people want to tidy are customer lists and bank
//! exports, and "this cannot send your spreadsheet anywhere" is a property the
//! host enforces here, not a promise in a privacy policy. Its Launchpad card
//! reads `OUTBOUND none`.
//!
//! Routes:
//!   GET  /           the browser UI
//!   POST /api/tidy   body = CSV text; returns { headers, rows, issues, stats }
//!   GET  /healthz    "ok"

use wasip3::http::types::{ErrorCode, Fields, Request, Response};
use wasip3::http_compat::{http_from_wasi_request, BodyWriter};
use http_body_util::BodyExt;

struct Component;

wasip3::http::service::export!(Component);

/// Bound on an upload. A component gets a fixed memory budget, and a CSV big
/// enough to exhaust it should be told so rather than taking the instance down.
const MAX_BODY: usize = 4 * 1024 * 1024;
/// Rows returned in the preview. The full count is still reported in `stats`.
const MAX_ROWS_OUT: usize = 2000;

/// A CSV parser over RFC 4180, plus the things real files do.
///
/// Handles quoted fields containing the delimiter, embedded newlines, and `""`
/// as an escaped quote. Accepts CRLF, LF and CR line endings. A quote appearing
/// inside an unquoted field is kept literally rather than treated as an error —
/// `O"Brien` in a hand-edited file should survive, not abort the parse.
fn parse_csv(input: &str, delim: char) -> Vec<Vec<String>> {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut chars = input.chars().peekable();
    let mut in_quotes = false;
    let mut field_started_quoted = false;

    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    in_quotes = false;
                }
            } else {
                field.push(c);
            }
            continue;
        }
        match c {
            '"' if field.is_empty() && !field_started_quoted => {
                in_quotes = true;
                field_started_quoted = true;
            }
            c if c == delim => {
                row.push(std::mem::take(&mut field));
                field_started_quoted = false;
            }
            '\r' => {
                // CRLF or a lone CR both end the record.
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
                field_started_quoted = false;
            }
            '\n' => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
                field_started_quoted = false;
            }
            c => field.push(c),
        }
    }
    // A file not ending in a newline still has a last record.
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows
}

/// Guess the delimiter by which candidate yields the most CONSISTENT column
/// count across the first few lines.
///
/// Counting occurrences is the obvious approach and it is wrong on the common
/// case: a comma-delimited file full of European decimals ("1,50") has more
/// semicolons per line than commas, or vice versa. Consistency is the property
/// that actually identifies a delimiter.
fn sniff_delimiter(sample: &str) -> char {
    const CANDIDATES: [char; 4] = [',', ';', '\t', '|'];
    let mut best = (',', 0usize, usize::MAX);
    for &d in &CANDIDATES {
        let rows = parse_csv(sample, d);
        let counts: Vec<usize> = rows.iter().filter(|r| !is_blank(r)).map(|r| r.len()).take(20).collect();
        if counts.is_empty() {
            continue;
        }
        let width = counts[0];
        if width < 2 {
            continue; // a delimiter that splits nothing is not the delimiter
        }
        let ragged = counts.iter().filter(|&&c| c != width).count();
        // Prefer fewer ragged rows; break ties on more columns.
        if ragged < best.2 || (ragged == best.2 && width > best.1) {
            best = (d, width, ragged);
        }
    }
    best.0
}

fn is_blank(row: &[String]) -> bool {
    row.iter().all(|f| f.trim().is_empty())
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn q(s: &str) -> String {
    format!("\"{}\"", json_escape(s))
}

struct Tidy {
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
    issues: Vec<String>,
    total_rows: usize,
    delimiter: char,
    blank_rows: usize,
}

fn tidy(input: &str) -> Tidy {
    let mut issues: Vec<String> = Vec::new();

    // A UTF-8 BOM makes the first header "\u{feff}Name", which then fails to
    // match "Name" everywhere downstream. It is invisible, so say so.
    let input = match input.strip_prefix('\u{feff}') {
        Some(rest) => {
            issues.push("Removed a UTF-8 byte-order mark from the start of the file.".into());
            rest
        }
        None => input,
    };

    let delimiter = sniff_delimiter(input);
    if delimiter != ',' {
        let name = match delimiter {
            ';' => "semicolon",
            '\t' => "tab",
            '|' => "pipe",
            _ => "comma",
        };
        issues.push(format!("Detected a {name}-delimited file, not comma-delimited."));
    }

    let raw = parse_csv(input, delimiter);
    let before = raw.len();
    /* Keep each row's ORIGINAL line number. The issues below name the rows that
       need looking at, and the whole value of naming one is that the reader can
       go and find it — so the number has to be the line in THEIR file, not the
       index after blank rows were dropped. A single blank line above a ragged
       row was enough to send them to the wrong place. */
    let mut numbered: Vec<(usize, Vec<String>)> = raw
        .into_iter()
        .enumerate()
        .map(|(i, r)| (i + 1, r.into_iter().map(|f| f.trim().to_string()).collect::<Vec<_>>()))
        .filter(|(_, r)| !is_blank(r))
        .collect();
    let blank_rows = before - numbered.len();
    if blank_rows > 0 {
        issues.push(format!(
            "Dropped {blank_rows} blank {}.",
            if blank_rows == 1 { "row" } else { "rows" }
        ));
    }

    if numbered.is_empty() {
        return Tidy { headers: vec![], rows: vec![], issues, total_rows: 0, delimiter, blank_rows };
    }

    // Header repair. An empty or duplicated name is the thing that breaks a
    // downstream import, and both are common in exports.
    let (_, mut headers) = numbered.remove(0);
    let mut seen: Vec<String> = Vec::new();
    let mut renamed = 0usize;
    let mut filled = 0usize;
    for (i, h) in headers.iter_mut().enumerate() {
        if h.is_empty() {
            *h = format!("column_{}", i + 1);
            filled += 1;
        }
        let base = h.clone();
        let mut n = 2;
        while seen.iter().any(|s| s.eq_ignore_ascii_case(h)) {
            *h = format!("{base}_{n}");
            n += 1;
            renamed += 1;
        }
        seen.push(h.clone());
    }
    if filled > 0 {
        issues.push(format!("Named {filled} empty header {}.", if filled == 1 { "column" } else { "columns" }));
    }
    if renamed > 0 {
        issues.push(format!("Renamed {renamed} duplicate header {}.", if renamed == 1 { "column" } else { "columns" }));
    }

    // Ragged rows. Report the first few line numbers: on a real export the
    // ragged row is usually the one worth looking at.
    let width = headers.len();
    let mut short: Vec<usize> = Vec::new();
    let mut long: Vec<usize> = Vec::new();
    for (line, r) in numbered.iter() {
        match r.len().cmp(&width) {
            std::cmp::Ordering::Less => short.push(*line),
            std::cmp::Ordering::Greater => long.push(*line),
            std::cmp::Ordering::Equal => {}
        }
    }
    let list = |v: &[usize]| {
        let shown: Vec<String> = v.iter().take(5).map(|n| n.to_string()).collect();
        if v.len() > 5 {
            format!("{} and {} more", shown.join(", "), v.len() - 5)
        } else {
            shown.join(", ")
        }
    };
    if !short.is_empty() {
        issues.push(format!(
            "{} {} fewer than {width} columns (row {}); padded with empty values.",
            short.len(),
            if short.len() == 1 { "row has" } else { "rows have" },
            list(&short)
        ));
    }
    if !long.is_empty() {
        issues.push(format!(
            "{} {} more than {width} columns (row {}); the extra values are kept as overflow_N.",
            long.len(),
            if long.len() == 1 { "row has" } else { "rows have" },
            list(&long)
        ));
    }

    // Normalise every row to the header width, keeping overflow rather than
    // discarding it — losing a value silently is worse than an odd column name.
    let mut rows: Vec<Vec<String>> = numbered.into_iter().map(|(_, r)| r).collect();
    for r in rows.iter_mut() {
        while r.len() < width {
            r.push(String::new());
        }
    }

    let total_rows = rows.len();
    if total_rows > MAX_ROWS_OUT {
        issues.push(format!(
            "Showing the first {MAX_ROWS_OUT} of {total_rows} rows; stats cover all of them."
        ));
    }
    Tidy { headers, rows, issues, total_rows, delimiter, blank_rows }
}

fn tidy_json(t: &Tidy) -> String {
    let width = t.headers.len();
    let headers = t.headers.iter().map(|h| q(h)).collect::<Vec<_>>().join(",");
    let rows = t
        .rows
        .iter()
        .take(MAX_ROWS_OUT)
        .map(|r| {
            let fields = r
                .iter()
                .enumerate()
                .map(|(i, v)| {
                    let key = if i < width {
                        t.headers[i].clone()
                    } else {
                        format!("overflow_{}", i - width + 1)
                    };
                    format!("{}:{}", q(&key), q(v))
                })
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{fields}}}")
        })
        .collect::<Vec<_>>()
        .join(",");
    let issues = t.issues.iter().map(|i| q(i)).collect::<Vec<_>>().join(",");
    let delim = match t.delimiter {
        '\t' => "\\t".to_string(),
        c => c.to_string(),
    };
    format!(
        "{{\"headers\":[{headers}],\"rows\":[{rows}],\"issues\":[{issues}],\
         \"stats\":{{\"columns\":{},\"rows\":{},\"blankRowsDropped\":{},\"delimiter\":\"{}\",\"rowsShown\":{}}}}}",
        width,
        t.total_rows,
        t.blank_rows,
        delim,
        t.rows.len().min(MAX_ROWS_OUT)
    )
}

const PAGE: &str = include_str!("index.html");

fn respond(status: u16, content_type: &str, body: String) -> Result<Response, ErrorCode> {
    let headers = Fields::from_list(&[
        ("content-type".to_string(), content_type.as_bytes().to_vec()),
        ("cache-control".to_string(), b"no-store".to_vec()),
        ("x-content-type-options".to_string(), b"nosniff".to_vec()),
    ])
    .map_err(|err| ErrorCode::InternalError(Some(format!("invalid headers: {err}"))))?;

    let (mut writer, body_rx, result_rx) = BodyWriter::new();
    let (response, _transmit) = Response::new(headers, Some(body_rx), result_rx);
    response
        .set_status_code(status)
        .map_err(|()| ErrorCode::InternalError(Some("invalid status code".into())))?;

    wasip3::wit_bindgen::spawn(async move {
        let frame = http_body::Frame::data(bytes::Bytes::from(body));
        let _ = writer.send_frame(frame).await;
        drop(writer.stream_writer);
        let _ = writer.result_writer.write(Ok(None)).await;
    });

    Ok(response)
}

impl wasip3::exports::http::handler::Guest for Component {
    async fn handle(request: Request) -> Result<Response, ErrorCode> {
        let request = http_from_wasi_request(request)?;
        let (parts, body) = request.into_parts();
        let path = parts.uri.path().to_string();
        let method = parts.method.clone();

        match (method.as_str(), path.as_str()) {
            ("GET", "/healthz") => respond(200, "text/plain; charset=utf-8", "ok\n".to_string()),
            ("GET", "/") | ("GET", "") => respond(200, "text/html; charset=utf-8", PAGE.to_string()),
            ("POST", "/api/tidy") => {
                let bytes = match http_body_util::Limited::new(body, MAX_BODY).collect().await {
                    Ok(collected) => collected.to_bytes(),
                    Err(_) => {
                        return respond(
                            413,
                            "application/json; charset=utf-8",
                            format!("{{\"error\":\"CSV is larger than {} MiB\"}}", MAX_BODY / (1024 * 1024)),
                        )
                    }
                };
                // Lossy rather than a 400: a Latin-1 export with one stray byte
                // should still tidy, with the bad byte visible as U+FFFD.
                let text = String::from_utf8_lossy(&bytes);
                if text.trim().is_empty() {
                    return respond(400, "application/json; charset=utf-8",
                        "{\"error\":\"the request body was empty; POST CSV text\"}".to_string());
                }
                respond(200, "application/json; charset=utf-8", tidy_json(&tidy(&text)))
            }
            ("POST", _) | ("GET", _) => {
                respond(404, "application/json; charset=utf-8", "{\"error\":\"not found\"}".to_string())
            }
            _ => respond(405, "application/json; charset=utf-8", "{\"error\":\"method not allowed\"}".to_string()),
        }
    }
}
