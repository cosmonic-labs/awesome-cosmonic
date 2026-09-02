//! The read-only statement inspector and identifier helpers.
//!
//! Every call may run on a different pooled connection, so `BEGIN READ ONLY`
//! (what the archived official postgres server did) cannot bracket a query
//! here: the transaction would end with the connection. Read-only mode is
//! therefore enforced **before** the statement reaches the host, by
//! inspecting it:
//!
//! 1. the text is tokenized with SQL lexical rules — single-quoted and
//!    `E'…'` strings, `$$`/`$tag$` dollar-quoted strings, `--` and nested
//!    `/* */` comments are skipped, so a keyword inside a literal cannot
//!    trip the check; `"quoted"` identifiers (and the `U&"…"` unicode-escape
//!    form, `UESCAPE` included) are kept as *quoted* tokens with their exact
//!    spelling — a quoted identifier can never be a keyword, so they take no
//!    part in the keyword checks, but they are still a valid way to call a
//!    function (`SELECT "nextval"(…)`), so they are matched against the
//!    side-effect function list;
//! 2. exactly one statement is allowed (`;` outside quotes, a trailing one
//!    tolerated);
//! 3. in read-only mode the leading keyword must be `SELECT`, `WITH`,
//!    `EXPLAIN`, `SHOW`, `VALUES` or `TABLE`; the statement `EXPLAIN` wraps
//!    (after its options) must not be DDL/maintenance (`CREATE TABLE AS`,
//!    …); and no token anywhere may be a data-modifying keyword (`INSERT`,
//!    `UPDATE`, `DELETE`, `MERGE` — CTE bodies — or `INTO`, for `SELECT …
//!    INTO new_table`), a locking clause (`FOR UPDATE`/`FOR SHARE`/`FOR NO
//!    KEY UPDATE`/`FOR KEY SHARE`), or a call to a side-effecting catalog
//!    function (`nextval`, `setval`, `pg_terminate_backend`, `set_config`,
//!    advisory locks, …);
//! 4. transaction control (`BEGIN`, `COMMIT`, `ROLLBACK`, `SAVEPOINT`,
//!    `START TRANSACTION`, `SET`, `RESET`, `PREPARE`, `DEALLOCATE`, `DECLARE`,
//!    `FETCH`, `LISTEN`, …) is refused in every mode: there is no session to
//!    carry it across calls, so it can only mislead.
//!
//! DDL and maintenance statements (`CREATE`, `DROP`, `COMMENT`, `VACUUM`,
//! `CALL`, …) are only matched at a statement head — that is the only place
//! Postgres grammar allows them — so the non-reserved words among them
//! (`comment`, `load`, `refresh`, `security`, …) can be used as identifiers
//! (`SELECT name AS comment …`) without a false positive.
//!
//! This is a heuristic, not a database guarantee — a user-defined function
//! with side effects called from a `SELECT` gets through, the non-reserved
//! words `insert`/`update`/`delete`/`merge` used as unquoted identifiers are
//! refused (quote or alias them), and a column or alias spelled exactly like
//! a listed side-effect function (`AS "nextval"`) is refused too. For real safety the
//! connection URL should use a role with `default_transaction_read_only =
//! on` (see the README). Rules borrowed from crystaldba/postgres-mcp's
//! restricted mode (MIT).

/// Why a statement was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The text is empty or only comments.
    Empty,
    /// More than one statement.
    MultipleStatements,
    /// Transaction/session control — meaningless without sessions.
    SessionControl(String),
    /// Not allowed while writes are disabled; carries the offending keyword.
    WriteInReadOnly(String),
    /// The first word is not a SQL statement keyword at all.
    NotAStatement(String),
}

impl Refusal {
    /// Client-facing message, actionable per case.
    pub fn message(&self, allow_writes: bool) -> String {
        match self {
            Refusal::Empty => "the statement is empty".to_owned(),
            Refusal::NotAStatement(w) => format!(
                "`{w}` is not a SQL statement keyword — a syntax error? Statements start with \
                 SELECT, WITH, EXPLAIN, SHOW, VALUES or TABLE (and, with writes enabled, INSERT, \
                 UPDATE, DELETE, CREATE, …). Wrap a parenthesised query as `SELECT * FROM (…) q`."
            ),
            Refusal::MultipleStatements => "only one statement per call is accepted (the host \
                 prepares every statement); send them one at a time, or use execute_batch for a \
                 multi-statement script when writes are enabled"
                .to_owned(),
            Refusal::SessionControl(kw) => format!(
                "`{}` is refused: there are no sessions — every call may run on a different \
                 pooled connection, so BEGIN/COMMIT, SET, PREPARE, DECLARE/FETCH and friends \
                 cannot carry across calls. Use execute_batch for an atomic multi-statement \
                 script, or SET LOCAL inside such a script.",
                kw.to_uppercase()
            ),
            Refusal::WriteInReadOnly(kw) => {
                let mut m = format!(
                    "read-only mode: `{}` is not allowed. This server runs with \
                     POSTGRES_ALLOW_WRITES unset (or not \"true\"), so only SELECT / WITH … SELECT \
                     / EXPLAIN / SHOW / VALUES / TABLE statements are accepted by `query`.",
                    kw.to_uppercase()
                );
                if !allow_writes {
                    m.push_str(
                        " To write, redeploy the workload with POSTGRES_ALLOW_WRITES=\"true\" \
                         (deploy/workload.yaml) and use `execute` / `execute_batch`; if this is a \
                         false positive (the word only appears inside an identifier), quote the \
                         identifier or alias it.",
                    );
                }
                m
            }
        }
    }
}

/// Leading keywords accepted in read-only mode.
const READ_ONLY_LEADING: &[&str] = &["select", "with", "explain", "show", "values", "table"];

/// Keywords that indicate a write anywhere in a statement: data-modifying
/// CTE bodies (`WITH d AS (DELETE …) SELECT …`), EXPLAIN bodies, and `SELECT
/// … INTO new_table`.
const WRITE_KEYWORDS_ANYWHERE: &[&str] = &["insert", "update", "delete", "merge", "into"];

/// Statements that are never read-only and can only appear at a statement
/// head — including the head of the statement `EXPLAIN` wraps (`EXPLAIN
/// CREATE TABLE … AS`, `EXPLAIN ANALYZE CREATE MATERIALIZED VIEW … AS`). They
/// are matched there only, so the non-reserved words among them can still be
/// identifiers elsewhere (`SELECT note AS comment …`).
const WRITE_STATEMENTS: &[&str] = &[
    "truncate",
    "drop",
    "create",
    "alter",
    "grant",
    "revoke",
    "copy",
    "vacuum",
    "analyze",
    "analyse",
    "reindex",
    "cluster",
    "refresh",
    "lock",
    "comment",
    "security",
    "import",
    "notify",
    "unlisten",
    "load",
    "checkpoint",
    "reassign",
    "call",
    "do",
];

/// Words that may sit between `EXPLAIN` and the statement it wraps: the
/// legacy and parenthesised options and their values (the lexer drops the
/// parentheses, commas and numbers). An unknown future option is harmless:
/// it merely hides the inner head from the head check, and the anywhere
/// scan still catches data-modifying statements.
const EXPLAIN_OPTIONS: &[&str] = &[
    "analyze",
    "analyse",
    "verbose",
    "costs",
    "settings",
    "generic_plan",
    "buffers",
    "serialize",
    "wal",
    "timing",
    "summary",
    "memory",
    "format",
    "text",
    "xml",
    "json",
    "yaml",
    "true",
    "false",
    "on",
    "off",
    "none",
    "binary",
];

/// Every keyword a Postgres statement can start with (after comments). A
/// leading word outside this list is a syntax error, not a policy decision.
const STATEMENT_KEYWORDS: &[&str] = &[
    "select",
    "with",
    "explain",
    "show",
    "values",
    "table",
    "insert",
    "update",
    "delete",
    "merge",
    "truncate",
    "drop",
    "create",
    "alter",
    "grant",
    "revoke",
    "copy",
    "vacuum",
    "analyze",
    "analyse",
    "reindex",
    "cluster",
    "refresh",
    "lock",
    "comment",
    "security",
    "import",
    "notify",
    "listen",
    "unlisten",
    "load",
    "checkpoint",
    "reassign",
    "call",
    "do",
    "begin",
    "commit",
    "rollback",
    "start",
    "savepoint",
    "release",
    "abort",
    "end",
    "set",
    "reset",
    "prepare",
    "deallocate",
    "execute",
    "discard",
    "declare",
    "fetch",
    "move",
    "close",
];

/// Statements (leading keyword) that only make sense inside a session.
const SESSION_KEYWORDS: &[&str] = &[
    "begin",
    "commit",
    "rollback",
    "start",
    "savepoint",
    "release",
    "abort",
    "end",
    "set",
    "reset",
    "prepare",
    "deallocate",
    "execute",
    "discard",
    "declare",
    "fetch",
    "move",
    "close",
    "listen",
];

/// Functions with side effects that a SELECT can call.
const SIDE_EFFECT_FUNCTIONS: &[&str] = &[
    "nextval",
    "setval",
    "lastval",
    "pg_terminate_backend",
    "pg_cancel_backend",
    "pg_reload_conf",
    "pg_rotate_logfile",
    "pg_advisory_lock",
    "pg_advisory_lock_shared",
    "pg_advisory_xact_lock",
    "pg_advisory_xact_lock_shared",
    "pg_try_advisory_lock",
    "pg_try_advisory_lock_shared",
    "pg_try_advisory_xact_lock",
    "pg_try_advisory_xact_lock_shared",
    "pg_advisory_unlock",
    "pg_advisory_unlock_all",
    "pg_advisory_unlock_shared",
    "set_config",
    "pg_notify",
    "pg_create_physical_replication_slot",
    "pg_create_logical_replication_slot",
    "pg_drop_replication_slot",
    "pg_switch_wal",
    "pg_promote",
    "pg_backup_start",
    "pg_backup_stop",
    "lo_import",
    "lo_export",
    "lo_unlink",
    "lo_create",
    "lo_from_bytea",
    "lo_put",
    "lowrite",
    "dblink_exec",
    "dblink",
    "pg_file_write",
    "pg_file_unlink",
    "pg_file_rename",
    "pg_sleep_for",
    "pg_sleep_until",
    "pg_stat_reset",
    "pg_stat_reset_shared",
    "pg_stat_reset_single_table_counters",
    "pg_stat_reset_single_function_counters",
    "pg_stat_reset_slru",
    "pg_stat_reset_replication_slot",
    "pg_stat_reset_subscription_stats",
    "pg_stat_statements_reset",
    "pg_stat_clear_snapshot",
    "pg_logical_emit_message",
    "pg_create_restore_point",
    "pg_replication_origin_create",
    "pg_replication_origin_drop",
    "pg_replication_origin_advance",
    "pg_replication_origin_session_setup",
    "pg_replication_origin_session_reset",
    "pg_replication_origin_xact_setup",
    "pg_replication_origin_xact_reset",
    "pg_replication_slot_advance",
    "pg_copy_physical_replication_slot",
    "pg_copy_logical_replication_slot",
    "pg_logical_slot_get_changes",
    "pg_logical_slot_get_binary_changes",
    "pg_wal_replay_pause",
    "pg_wal_replay_resume",
    "pg_log_backend_memory_contexts",
    "pg_import_system_collations",
    "pg_prewarm",
    "brin_summarize_new_values",
    "brin_summarize_range",
    "brin_desummarize_range",
    "gin_clean_pending_list",
    "dblink_connect",
    "dblink_connect_u",
    "dblink_disconnect",
    "dblink_send_query",
    "lo_open",
    "lo_truncate",
    "lo_truncate64",
];

/// What the inspector found out about a statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inspection {
    /// Lower-cased leading keyword.
    pub leading: String,
    /// Whether the statement passes the read-only rules.
    pub read_only: bool,
    /// The first keyword that made it non-read-only, if any.
    pub write_keyword: Option<String>,
}

/// One word of a statement.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Token {
    /// Unquoted words are lower-cased (Postgres folds them); quoted
    /// identifiers keep their exact spelling (`""` unescaped, `U&` escapes
    /// decoded).
    text: String,
    /// `"quoted"` (or `U&"…"`): never a keyword, possibly a function name.
    quoted: bool,
}

impl Token {
    /// The word as an unquoted keyword/identifier, or `None` when quoted.
    fn word(&self) -> Option<&str> {
        (!self.quoted).then_some(self.text.as_str())
    }
}

/// Lexes the statement into word tokens (keywords, identifiers, function
/// names — quoted identifiers flagged) plus the number of statement
/// separators found, skipping literals and comments.
fn tokens(sql: &str) -> Result<(Vec<Token>, usize), Refusal> {
    let bytes = sql.as_bytes();
    let mut i = 0usize;
    let mut words: Vec<Token> = Vec::new();
    let mut separators = 0usize;
    let mut last_significant_was_separator = false;
    let mut saw_anything = false;

    while i < bytes.len() {
        let c = bytes[i];
        // Comments.
        if c == b'-' && bytes.get(i + 1) == Some(&b'-') {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'/' && bytes.get(i + 1) == Some(&b'*') {
            let mut depth = 1usize;
            i += 2;
            while i < bytes.len() && depth > 0 {
                if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                    depth += 1;
                    i += 2;
                } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            continue;
        }
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        saw_anything = true;
        // Standard and escape string literals ('' doubles a quote inside).
        if c == b'\'' || ((c == b'E' || c == b'e') && bytes.get(i + 1) == Some(&b'\'')) {
            let escape = c != b'\'';
            i += if escape { 2 } else { 1 };
            while i < bytes.len() {
                if escape && bytes[i] == b'\\' {
                    i += 2;
                    continue;
                }
                if bytes[i] == b'\'' {
                    if bytes.get(i + 1) == Some(&b'\'') {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            last_significant_was_separator = false;
            continue;
        }
        // Quoted identifiers: kept as tokens, flagged. `U&"…"` is handled
        // in the word branch below.
        if c == b'"' {
            let (name, next) = lex_quoted_ident(sql, i + 1);
            i = next;
            words.push(Token {
                text: name,
                quoted: true,
            });
            last_significant_was_separator = false;
            continue;
        }
        // Dollar-quoted strings: $$ … $$ or $tag$ … $tag$ (a bare `$1`
        // placeholder is a digit-tag and never a quote).
        if c == b'$' {
            let mut j = i + 1;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            let tag_is_placeholder = bytes.get(i + 1).is_some_and(|b| b.is_ascii_digit());
            if !tag_is_placeholder && bytes.get(j) == Some(&b'$') {
                let tag = &bytes[i..=j];
                let mut k = j + 1;
                loop {
                    if k + tag.len() > bytes.len() {
                        k = bytes.len();
                        break;
                    }
                    if &bytes[k..k + tag.len()] == tag {
                        k += tag.len();
                        break;
                    }
                    k += 1;
                }
                i = k;
                last_significant_was_separator = false;
                continue;
            }
            i = j.max(i + 1);
            last_significant_was_separator = false;
            continue;
        }
        if c == b';' {
            separators += 1;
            last_significant_was_separator = true;
            i += 1;
            continue;
        }
        // Words: keywords, identifiers, function names.
        if c.is_ascii_alphabetic() || c == b'_' {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let word = sql[start..i].to_ascii_lowercase();
            // `U&"…"` — a unicode-escaped quoted identifier, optionally
            // followed by `UESCAPE 'x'`. Decoded so `U&"\006eextval"` is
            // seen as `nextval`.
            if word == "u" && bytes.get(i) == Some(&b'&') && bytes.get(i + 1) == Some(&b'"') {
                let (raw, next) = lex_quoted_ident(sql, i + 2);
                let (escape, next) = lex_uescape(sql, next);
                i = next;
                words.push(Token {
                    text: decode_unicode_escapes(&raw, escape),
                    quoted: true,
                });
                last_significant_was_separator = false;
                continue;
            }
            words.push(Token {
                text: word,
                quoted: false,
            });
            last_significant_was_separator = false;
            continue;
        }
        // Numbers, operators, punctuation, non-ASCII: skip one char.
        let width = utf8_width(c);
        i += width;
        last_significant_was_separator = false;
    }

    if !saw_anything || words.is_empty() {
        return Err(Refusal::Empty);
    }
    // A trailing `;` is not a second statement.
    if last_significant_was_separator && separators > 0 {
        separators -= 1;
    }
    Ok((words, separators))
}

fn utf8_width(first: u8) -> usize {
    if first < 0x80 {
        1
    } else if first >> 5 == 0b110 {
        2
    } else if first >> 4 == 0b1110 {
        3
    } else {
        4
    }
}

/// Lexes a double-quoted identifier whose opening quote sits just before
/// `start`; returns its contents (`""` unescaped) and the index after the
/// closing quote (or the end of the text when unterminated). `start` is
/// always just past an ASCII `"`, so every cut is on a char boundary.
fn lex_quoted_ident(sql: &str, start: usize) -> (String, usize) {
    let bytes = sql.as_bytes();
    let mut i = start;
    let mut name = String::new();
    let mut seg = i;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            name.push_str(sql.get(seg..i).unwrap_or(""));
            if bytes.get(i + 1) == Some(&b'"') {
                name.push('"');
                i += 2;
                seg = i;
                continue;
            }
            return (name, i + 1);
        }
        i += 1;
    }
    name.push_str(sql.get(seg..).unwrap_or(""));
    (name, bytes.len())
}

/// After a `U&"…"` identifier, an optional `UESCAPE 'x'` clause selects the
/// escape character (default `\`). Returns the escape and the index after
/// the clause (or `start` when there is none).
fn lex_uescape(sql: &str, start: usize) -> (char, usize) {
    let bytes = sql.as_bytes();
    let mut i = start;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    let kw = b"uescape";
    let matches_kw = bytes
        .get(i..i + kw.len())
        .is_some_and(|w| w.eq_ignore_ascii_case(kw));
    if !matches_kw {
        return ('\\', start);
    }
    let mut j = i + kw.len();
    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
        j += 1;
    }
    if bytes.get(j) != Some(&b'\'') {
        return ('\\', start);
    }
    let rest = sql.get(j + 1..).unwrap_or("");
    let mut chars = rest.chars();
    match (chars.next(), chars.next()) {
        (Some(esc), Some('\'')) if esc != '\'' => (esc, j + 1 + esc.len_utf8() + 1),
        _ => ('\\', start),
    }
}

/// Decodes the `\XXXX` / `\+XXXXXX` escapes of a `U&"…"` identifier (`\\`
/// is a literal escape character); anything malformed is kept as written
/// (Postgres would reject the statement anyway).
fn decode_unicode_escapes(raw: &str, escape: char) -> String {
    let mut out = String::with_capacity(raw.len());
    let chars: Vec<char> = raw.chars().collect();
    let mut i = 0usize;
    let hex = |slice: &[char]| -> Option<char> {
        let s: String = slice.iter().collect();
        u32::from_str_radix(&s, 16).ok().and_then(char::from_u32)
    };
    while i < chars.len() {
        let c = chars[i];
        if c != escape {
            out.push(c);
            i += 1;
            continue;
        }
        if chars.get(i + 1) == Some(&escape) {
            out.push(escape);
            i += 2;
        } else if chars.get(i + 1) == Some(&'+') {
            match chars.get(i + 2..i + 8).and_then(hex) {
                Some(decoded) => {
                    out.push(decoded);
                    i += 8;
                }
                None => {
                    out.push(c);
                    i += 1;
                }
            }
        } else {
            match chars.get(i + 1..i + 5).and_then(hex) {
                Some(decoded) => {
                    out.push(decoded);
                    i += 5;
                }
                None => {
                    out.push(c);
                    i += 1;
                }
            }
        }
    }
    out
}

/// Inspects one statement. `Ok` describes it; `Err` is a refusal that applies
/// regardless of mode (empty, multiple statements, session control).
pub fn inspect(sql: &str) -> Result<Inspection, Refusal> {
    let (words, separators) = tokens(sql)?;
    if separators > 0 {
        return Err(Refusal::MultipleStatements);
    }
    // A quoted first token is an identifier, never a statement keyword.
    let leading = words
        .first()
        .and_then(Token::word)
        .map(str::to_owned)
        .unwrap_or_default();
    if !STATEMENT_KEYWORDS.contains(&leading.as_str()) {
        return Err(Refusal::NotAStatement(leading));
    }
    if SESSION_KEYWORDS.contains(&leading.as_str()) {
        return Err(Refusal::SessionControl(leading));
    }
    let write_keyword = find_write_keyword(&words);
    Ok(Inspection {
        read_only: write_keyword.is_none(),
        leading,
        write_keyword,
    })
}

/// The first token that makes the statement non-read-only, if any.
fn find_write_keyword(words: &[Token]) -> Option<String> {
    let leading = words.first()?.word()?;
    if !READ_ONLY_LEADING.contains(&leading) {
        return Some(leading.to_owned());
    }
    // The statement EXPLAIN wraps is a statement head too: skip the option
    // words and check what follows. (EXPLAIN ANALYZE executes it, and even a
    // plain EXPLAIN of `CREATE TABLE … AS` is DDL territory.) A
    // data-modifying inner statement is caught by the anywhere scan below.
    if leading == "explain" {
        let inner = words
            .iter()
            .skip(1)
            .find(|t| !t.word().is_some_and(|w| EXPLAIN_OPTIONS.contains(&w)))
            .and_then(Token::word);
        if let Some(inner) = inner.filter(|w| WRITE_STATEMENTS.contains(w)) {
            return Some(inner.to_owned());
        }
    }
    let word_at = |i: usize| words.get(i).and_then(Token::word);
    for (i, t) in words.iter().enumerate() {
        // A quoted identifier is never a keyword, but `"nextval"(…)` is a
        // call of the same function as `nextval(…)`: match it by its exact
        // (case-sensitive) spelling, the way Postgres resolves it.
        let Some(w) = t.word() else {
            if SIDE_EFFECT_FUNCTIONS.contains(&t.text.as_str()) {
                return Some(t.text.clone());
            }
            continue;
        };
        if WRITE_KEYWORDS_ANYWHERE.contains(&w) {
            return Some(w.to_owned());
        }
        if SIDE_EFFECT_FUNCTIONS.contains(&w) {
            return Some(w.to_owned());
        }
        // Locking clauses: FOR UPDATE | FOR NO KEY UPDATE | FOR SHARE | FOR KEY SHARE.
        if w == "for" {
            let next = word_at(i + 1);
            let next2 = word_at(i + 2);
            let next3 = word_at(i + 3);
            let locking = matches!(next, Some("update") | Some("share"))
                || (next == Some("key") && next2 == Some("share"))
                || (next == Some("no") && next2 == Some("key") && next3 == Some("update"));
            if locking {
                return Some("for update/share".to_owned());
            }
        }
    }
    None
}

/// Whether a statement is (probably) an EXPLAIN already — `explain_query`
/// refuses those so the wrapper is not doubled.
pub fn starts_with_explain(sql: &str) -> bool {
    tokens(sql)
        .map(|(words, _)| words.first().and_then(Token::word) == Some("explain"))
        .unwrap_or(false)
}

/// Double-quotes an identifier for interpolation into SQL (`"` doubled).
pub fn quote_ident(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 2);
    out.push('"');
    for c in name.chars() {
        if c == '"' {
            out.push('"');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// Splits a user-supplied table reference into `(schema, table)`: `t`,
/// `s.t`, `"S"."T"`, `"a.b".t`. Quotes are stripped (and `""` unescaped);
/// unquoted parts are kept as written — Postgres folds unquoted identifiers
/// to lower case, and callers pass the catalog's stored name, so no folding
/// happens here.
pub fn split_table_ref(reference: &str) -> Option<(Option<String>, String)> {
    let s = reference.trim();
    if s.is_empty() {
        return None;
    }
    let mut parts: Vec<String> = Vec::new();
    let mut chars = s.chars().peekable();
    let mut current = String::new();
    let mut quoted = false;
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                if quoted {
                    if chars.peek() == Some(&'"') {
                        chars.next();
                        current.push('"');
                    } else {
                        quoted = false;
                    }
                } else {
                    quoted = true;
                }
            }
            '.' if !quoted => {
                parts.push(std::mem::take(&mut current));
            }
            _ => current.push(c),
        }
    }
    if quoted {
        return None;
    }
    parts.push(current);
    if parts.iter().any(String::is_empty) {
        return None;
    }
    match parts.len() {
        1 => Some((None, parts.remove(0))),
        2 => {
            let table = parts.remove(1);
            Some((Some(parts.remove(0)), table))
        }
        _ => None,
    }
}

/// Escapes `%`, `_` and `\` so a user pattern matches literally inside
/// `ILIKE '%' || $1 || '%' ESCAPE '\'`.
pub fn escape_like(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len() + 4);
    for c in pattern.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}
