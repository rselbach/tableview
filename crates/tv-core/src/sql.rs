use crate::connection::DbEngine;

/// A single non-empty SQL statement slice with byte offsets into its source editor buffer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SqlStatement {
    pub sql: String,
    pub start_byte: usize,
    pub end_byte: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScanState {
    Normal,
    SingleQuote,
    DoubleQuote,
    BacktickQuote,
    LineComment,
    BlockComment(usize),
}

/// Splits PostgreSQL SQL text on top-level `;` while respecting quotes, `--` comments,
/// nested `/* ... */` comments, and `$tag$...$tag$` dollar-quoted blocks.
pub fn split_statements(input: &str) -> Vec<SqlStatement> {
    split_statements_for_engine(input, DbEngine::Postgres)
}

/// Splits SQL text on top-level `;` using dialect-specific quoting and comment rules.
pub fn split_statements_for_engine(input: &str, engine: DbEngine) -> Vec<SqlStatement> {
    let bytes = input.as_bytes();
    let len = bytes.len();
    let mut out = Vec::new();
    let mut stmt_start = 0usize;
    let mut i = 0usize;
    let mut state = ScanState::Normal;
    let mut dollar_tag: Option<String> = None;

    let allow_backtick = matches!(engine, DbEngine::Mysql | DbEngine::Sqlite);
    let allow_hash_comment = engine == DbEngine::Mysql;
    let allow_backslash_escape = engine == DbEngine::Mysql;
    let allow_dollar_quote = engine == DbEngine::Postgres;

    while i < len {
        if let Some(ref tag) = dollar_tag {
            if bytes[i] == b'$' && input[i..].starts_with(tag.as_str()) {
                i += tag.len();
                dollar_tag = None;
            } else {
                i += char_len_at(input, i);
            }
            continue;
        }

        match state {
            ScanState::Normal => {
                if bytes[i] == b'\'' {
                    state = ScanState::SingleQuote;
                    i += 1;
                } else if bytes[i] == b'"' {
                    state = ScanState::DoubleQuote;
                    i += 1;
                } else if allow_backtick && bytes[i] == b'`' {
                    state = ScanState::BacktickQuote;
                    i += 1;
                } else if i + 1 < len && bytes[i] == b'-' && bytes[i + 1] == b'-' {
                    state = ScanState::LineComment;
                    i += 2;
                } else if allow_hash_comment && bytes[i] == b'#' {
                    state = ScanState::LineComment;
                    i += 1;
                } else if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
                    state = ScanState::BlockComment(1);
                    i += 2;
                } else if allow_dollar_quote
                    && bytes[i] == b'$'
                    && let Some(tag) = parse_dollar_tag(&input[i..])
                {
                    i += tag.len();
                    dollar_tag = Some(tag.to_owned());
                } else if bytes[i] == b';' {
                    push_trimmed_statement(input, stmt_start, i, engine, &mut out);
                    i += 1;
                    stmt_start = i;
                } else {
                    i += char_len_at(input, i);
                }
            }
            ScanState::SingleQuote => {
                if allow_backslash_escape && bytes[i] == b'\\' {
                    i += if i + 1 < len { 2 } else { 1 };
                } else if bytes[i] == b'\'' {
                    if i + 1 < len && bytes[i + 1] == b'\'' {
                        i += 2;
                    } else {
                        state = ScanState::Normal;
                        i += 1;
                    }
                } else {
                    i += char_len_at(input, i);
                }
            }
            ScanState::DoubleQuote => {
                if bytes[i] == b'"' {
                    if i + 1 < len && bytes[i + 1] == b'"' {
                        i += 2;
                    } else {
                        state = ScanState::Normal;
                        i += 1;
                    }
                } else {
                    i += char_len_at(input, i);
                }
            }
            ScanState::BacktickQuote => {
                if bytes[i] == b'`' {
                    if i + 1 < len && bytes[i + 1] == b'`' {
                        i += 2;
                    } else {
                        state = ScanState::Normal;
                        i += 1;
                    }
                } else {
                    i += char_len_at(input, i);
                }
            }
            ScanState::LineComment => {
                if bytes[i] == b'\n' {
                    state = ScanState::Normal;
                }
                i += char_len_at(input, i);
            }
            ScanState::BlockComment(depth) => {
                if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
                    state = ScanState::BlockComment(depth + 1);
                    i += 2;
                } else if i + 1 < len && bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    state = if depth <= 1 {
                        ScanState::Normal
                    } else {
                        ScanState::BlockComment(depth - 1)
                    };
                    i += 2;
                } else {
                    i += char_len_at(input, i);
                }
            }
        }
    }

    if stmt_start < len {
        push_trimmed_statement(input, stmt_start, len, engine, &mut out);
    }

    out
}

/// Returns the active selection if non-empty, or the PostgreSQL statement containing the caret.
pub fn statement_at_cursor(input: &str, anchor: usize, focus: usize) -> Option<SqlStatement> {
    statement_at_cursor_for_engine(input, anchor, focus, DbEngine::Postgres)
}

/// Returns the active selection if non-empty, or the dialect-aware statement containing the caret.
pub fn statement_at_cursor_for_engine(
    input: &str,
    anchor: usize,
    focus: usize,
    engine: DbEngine,
) -> Option<SqlStatement> {
    let a = clamp_char_boundary(input, anchor);
    let b = clamp_char_boundary(input, focus);
    if a != b {
        let lo = a.min(b);
        let hi = a.max(b);
        let (start_byte, end_byte, trimmed) = trim_span(input, lo, hi);
        if has_executable_sql_for_engine(trimmed, engine) {
            return Some(SqlStatement {
                sql: trimmed.to_owned(),
                start_byte,
                end_byte,
            });
        }
        return None;
    }

    let stmts = split_statements_for_engine(input, engine);
    if stmts.is_empty() {
        return None;
    }

    for stmt in &stmts {
        // Treat cursor resting immediately before or on the statement's trailing `;` as inside it.
        let end_with_semi = input[stmt.end_byte..]
            .chars()
            .next()
            .filter(|&c| c == ';')
            .map_or(stmt.end_byte, |c| stmt.end_byte + c.len_utf8());
        if b >= stmt.start_byte && b <= end_with_semi {
            return Some(stmt.clone());
        }
    }

    // If cursor sits after a semicolon on the same line (no newline yet), prefer that statement.
    let mut prev_on_line = None;
    for stmt in &stmts {
        if stmt.end_byte < b {
            let between = &input[stmt.end_byte..b];
            if !between.contains('\n') {
                prev_on_line = Some(stmt.clone());
            }
        }
    }
    if prev_on_line.is_some() {
        return prev_on_line;
    }

    if let Some(after) = stmts.iter().find(|s| s.start_byte > b) {
        return Some(after.clone());
    }
    stmts.last().cloned()
}

/// Computes 1-based `(line, column)` from a UTF-8 byte offset inside `input`.
pub fn cursor_line_col(input: &str, byte_offset: usize) -> (usize, usize) {
    let clamped = clamp_char_boundary(input, byte_offset);
    let prefix = &input[..clamped];
    let line = 1 + prefix.bytes().filter(|&b| b == b'\n').count();
    let last_line = prefix.rsplit('\n').next().unwrap_or(prefix);
    let col = 1 + last_line.chars().count();
    (line, col)
}

/// Formats SQL by uppercasing PostgreSQL keywords and breaking major top-level clauses cleanly
/// without altering string literals, quoted identifiers, dollar-quoted bodies, or comments.
pub fn format_sql(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    let bytes = trimmed.as_bytes();
    let len = bytes.len();
    let mut toks: Vec<FormatTok> = Vec::new();
    let mut i = 0usize;

    while i < len {
        let ch = trimmed[i..].chars().next().unwrap_or('\0');
        if ch.is_whitespace() {
            let start = i;
            while i < len {
                let c = trimmed[i..].chars().next().unwrap_or('\0');
                if !c.is_whitespace() {
                    break;
                }
                i += c.len_utf8();
            }
            toks.push(FormatTok::Space(trimmed[start..i].to_owned()));
        } else if ch == '\'' {
            let start = i;
            i += 1;
            while i < len {
                if bytes[i] == b'\'' {
                    if i + 1 < len && bytes[i + 1] == b'\'' {
                        i += 2;
                    } else {
                        i += 1;
                        break;
                    }
                } else {
                    i += char_len_at(trimmed, i);
                }
            }
            toks.push(FormatTok::Raw(trimmed[start..i].to_owned()));
        } else if ch == '"' {
            let start = i;
            i += 1;
            while i < len {
                if bytes[i] == b'"' {
                    if i + 1 < len && bytes[i + 1] == b'"' {
                        i += 2;
                    } else {
                        i += 1;
                        break;
                    }
                } else {
                    i += char_len_at(trimmed, i);
                }
            }
            toks.push(FormatTok::Raw(trimmed[start..i].to_owned()));
        } else if ch == '`' {
            let start = i;
            i += 1;
            while i < len {
                if bytes[i] == b'`' {
                    if i + 1 < len && bytes[i + 1] == b'`' {
                        i += 2;
                    } else {
                        i += 1;
                        break;
                    }
                } else {
                    i += char_len_at(trimmed, i);
                }
            }
            toks.push(FormatTok::Raw(trimmed[start..i].to_owned()));
        } else if i + 1 < len && bytes[i] == b'-' && bytes[i + 1] == b'-' {
            let start = i;
            i += 2;
            while i < len && bytes[i] != b'\n' {
                i += char_len_at(trimmed, i);
            }
            toks.push(FormatTok::Raw(trimmed[start..i].to_owned()));
        } else if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            let start = i;
            i += 2;
            let mut depth = 1usize;
            while i < len && depth > 0 {
                if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
                    depth += 1;
                    i += 2;
                } else if i + 1 < len && bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    depth -= 1;
                    i += 2;
                } else {
                    i += char_len_at(trimmed, i);
                }
            }
            toks.push(FormatTok::Raw(trimmed[start..i].to_owned()));
        } else if ch == '$'
            && let Some(tag) = parse_dollar_tag(&trimmed[i..])
        {
            let start = i;
            i += tag.len();
            while i < len {
                if bytes[i] == b'$' && trimmed[i..].starts_with(tag) {
                    i += tag.len();
                    break;
                }
                i += char_len_at(trimmed, i);
            }
            toks.push(FormatTok::Raw(trimmed[start..i].to_owned()));
        } else if ch.is_ascii_alphanumeric() || ch == '_' {
            let start = i;
            while i < len {
                let c = trimmed[i..].chars().next().unwrap_or('\0');
                if !(c.is_ascii_alphanumeric() || c == '_') {
                    break;
                }
                i += c.len_utf8();
            }
            let word = &trimmed[start..i];
            if is_sql_keyword(word) {
                toks.push(FormatTok::Word(word.to_ascii_uppercase()));
            } else {
                toks.push(FormatTok::Raw(word.to_owned()));
            }
        } else {
            i += ch.len_utf8();
            toks.push(FormatTok::Punct(ch));
        }
    }

    let mut out = String::with_capacity(trimmed.len() + 32);
    let mut paren_depth = 0usize;
    let mut at_line_start = true;
    let mut pending_space = false;

    let mut idx = 0usize;
    while idx < toks.len() {
        match &toks[idx] {
            FormatTok::Space(ws) => {
                if ws.contains('\n') {
                    if !at_line_start {
                        out.push('\n');
                        at_line_start = true;
                    }
                    pending_space = false;
                } else if !at_line_start {
                    pending_space = true;
                }
                idx += 1;
            }
            FormatTok::Punct(ch) => {
                let c = *ch;
                if c == '(' {
                    if pending_space {
                        out.push(' ');
                        pending_space = false;
                    }
                    out.push('(');
                    paren_depth += 1;
                    at_line_start = false;
                } else if c == ')' {
                    paren_depth = paren_depth.saturating_sub(1);
                    pending_space = false;
                    out.push(')');
                    at_line_start = false;
                } else if c == ';' {
                    pending_space = false;
                    out.push(';');
                    let has_more = toks[idx + 1..].iter().any(|t| !matches!(t, FormatTok::Space(_)));
                    if has_more {
                        out.push('\n');
                        at_line_start = true;
                    } else {
                        at_line_start = false;
                    }
                } else if c == ',' || c == '.' {
                    pending_space = c == ',';
                    out.push(c);
                    at_line_start = false;
                } else {
                    if pending_space {
                        out.push(' ');
                        pending_space = false;
                    }
                    out.push(c);
                    at_line_start = false;
                }
                idx += 1;
            }
            FormatTok::Word(kw) => {
                let next_word = next_non_space_word(&toks, idx + 1);
                let prev_non_space_was_join_mod = prev_non_space_word(&toks, idx)
                    .is_some_and(|w| matches!(w, "LEFT" | "RIGHT" | "FULL" | "INNER" | "CROSS" | "OUTER"));
                let is_clause_start = paren_depth == 0
                    && match kw.as_str() {
                        "SELECT" if idx > 0 => true,
                        "FROM" | "WHERE" | "HAVING" | "LIMIT" | "OFFSET" | "RETURNING" | "UNION" | "VALUES"
                        | "SET" => true,
                        "GROUP" | "ORDER" if next_word == Some("BY") => true,
                        "LEFT" | "RIGHT" | "INNER" | "CROSS" | "FULL" if next_word == Some("JOIN") || next_word == Some("OUTER") => true,
                        "JOIN" if !prev_non_space_was_join_mod => true,
                        _ => false,
                    };

                if is_clause_start && !at_line_start {
                    out.push('\n');
                    pending_space = false;
                } else if pending_space {
                    out.push(' ');
                    pending_space = false;
                }
                out.push_str(kw);
                at_line_start = false;
                idx += 1;
            }
            FormatTok::Raw(raw) => {
                if pending_space {
                    out.push(' ');
                    pending_space = false;
                }
                out.push_str(raw);
                at_line_start = false;
                idx += 1;
            }
        }
    }

    return_clean_formatted(out)
}

fn return_clean_formatted(mut out: String) -> String {
    while out.ends_with('\n') || out.ends_with(' ') {
        out.pop();
    }
    out
}

fn next_non_space_word(toks: &[impl WordInspect], from: usize) -> Option<&str> {
    toks[from..]
        .iter()
        .find(|t| !t.is_space())
        .and_then(|t| t.word_str())
}

fn prev_non_space_word(toks: &[impl WordInspect], before: usize) -> Option<&str> {
    toks[..before]
        .iter()
        .rev()
        .find(|t| !t.is_space())
        .and_then(|t| t.word_str())
}

trait WordInspect {
    fn is_space(&self) -> bool;
    fn word_str(&self) -> Option<&str>;
}

// Re-structured helper using generic free functions directly on the local enum isn't valid outside format_sql;
// provide a tiny module-private enum instead? Let's define FormatTok cleanly at module scope!
#[derive(Clone, Debug)]
enum FormatTok {
    Space(String),
    Word(String),
    Punct(char),
    Raw(String),
}

impl WordInspect for FormatTok {
    fn is_space(&self) -> bool {
        matches!(self, Self::Space(_))
    }

    fn word_str(&self) -> Option<&str> {
        match self {
            Self::Word(w) => Some(w.as_str()),
            _ => None,
        }
    }
}

fn is_sql_keyword(word: &str) -> bool {
    const KEYWORDS: &[&str] = &[
        "ALL", "ALTER", "ANALYZE", "AND", "AS", "ASC", "ATTACH", "BEGIN", "BETWEEN", "BY", "CASE", "CAST",
        "COALESCE", "COMMIT", "CONFLICT", "CONSTRAINT", "CREATE", "CROSS", "DEFAULT", "DELETE", "DESC",
        "DESCRIBE", "DETACH", "DISTINCT", "DO", "DROP", "ELSE", "END", "EXCEPT", "EXISTS", "EXPLAIN", "FALSE",
        "FETCH", "FIRST", "FOREIGN", "FROM", "FULL", "GROUP", "HAVING", "ILIKE", "IN", "INDEX", "INNER",
        "INSERT", "INTERSECT", "INTO", "IS", "JOIN", "KEY", "LAST", "LEFT", "LIKE", "LIMIT", "MATERIALIZED",
        "NOT", "NULL", "NULLS", "OFFSET", "ON", "ONLY", "OR", "ORDER", "OUTER", "OVER", "PARTITION", "PRAGMA",
        "PRIMARY", "REFERENCES", "REPLACE", "RETURNING", "RIGHT", "ROLLBACK", "SELECT", "SET", "SHOW", "TABLE",
        "THEN", "TRUE", "UNION", "UNIQUE", "UPDATE", "USE", "USING", "VALUES", "VIEW", "WHEN", "WHERE", "WITH",
    ];
    KEYWORDS.iter().any(|kw| kw.eq_ignore_ascii_case(word))
}

fn parse_dollar_tag(slice: &str) -> Option<&str> {
    let rest = slice.strip_prefix('$')?;
    let end = rest.find('$')?;
    let inner = &rest[..end];
    if inner.is_empty()
        || (inner.bytes().next().is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
            && inner.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
    {
        return Some(&slice[..end + 2]);
    }
    None
}

fn push_trimmed_statement(
    input: &str,
    raw_start: usize,
    raw_end: usize,
    engine: DbEngine,
    out: &mut Vec<SqlStatement>,
) {
    let (start_byte, end_byte, trimmed) = trim_span(input, raw_start, raw_end);
    if has_executable_sql_for_engine(trimmed, engine) {
        out.push(SqlStatement {
            sql: trimmed.to_owned(),
            start_byte,
            end_byte,
        });
    }
}

fn trim_span(input: &str, mut start: usize, mut end: usize) -> (usize, usize, &str) {
    while start < end {
        let ch = input[start..end].chars().next().unwrap_or('\0');
        if !ch.is_whitespace() {
            break;
        }
        start += ch.len_utf8();
    }
    while end > start {
        let ch = input[start..end].chars().next_back().unwrap_or('\0');
        if !ch.is_whitespace() {
            break;
        }
        end -= ch.len_utf8();
    }
    (start, end, &input[start..end])
}

/// Returns `true` if `sql` contains at least one non-whitespace token outside comments.
pub fn has_executable_sql(sql: &str) -> bool {
    has_executable_sql_for_engine(sql, DbEngine::Postgres)
}

/// Returns `true` if `sql` contains at least one non-whitespace token outside dialect comments.
pub fn has_executable_sql_for_engine(sql: &str, engine: DbEngine) -> bool {
    let bytes = sql.as_bytes();
    let len = bytes.len();
    let allow_hash = engine == DbEngine::Mysql;
    let mut i = 0usize;
    while i < len {
        let ch = sql[i..].chars().next().unwrap_or('\0');
        if ch.is_whitespace() {
            i += ch.len_utf8();
        } else if i + 1 < len && bytes[i] == b'-' && bytes[i + 1] == b'-' {
            i += 2;
            while i < len && bytes[i] != b'\n' {
                i += char_len_at(sql, i);
            }
        } else if allow_hash && bytes[i] == b'#' {
            i += 1;
            while i < len && bytes[i] != b'\n' {
                i += char_len_at(sql, i);
            }
        } else if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            let mut depth = 1usize;
            while i < len && depth > 0 {
                if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
                    depth += 1;
                    i += 2;
                } else if i + 1 < len && bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    depth -= 1;
                    i += 2;
                } else {
                    i += char_len_at(sql, i);
                }
            }
        } else {
            return true;
        }
    }
    false
}

fn char_len_at(s: &str, idx: usize) -> usize {
    s[idx..].chars().next().map_or(1, char::len_utf8)
}

fn clamp_char_boundary(s: &str, idx: usize) -> usize {
    let mut pos = idx.min(s.len());
    while pos > 0 && !s.is_char_boundary(pos) {
        pos -= 1;
    }
    pos
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitter_handles_mysql_and_sqlite_dialects() {
        let mysql_sql = "SELECT 'a\\';b', `col;1` FROM `tbl`; # hash ; comment\nSELECT 2;";
        let stmts = split_statements_for_engine(mysql_sql, DbEngine::Mysql);
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[0].sql, "SELECT 'a\\';b', `col;1` FROM `tbl`");
        assert!(stmts[1].sql.ends_with("SELECT 2"));

        // Postgres must NOT treat \' as an escape inside single quotes or # as a comment.
        let pg_sql = "SELECT 'a\\' ; SELECT 2;";
        let pg_stmts = split_statements_for_engine(pg_sql, DbEngine::Postgres);
        assert_eq!(pg_stmts.len(), 2);

        let sqlite_sql = "PRAGMA table_info(`my``tbl`); SELECT 1;";
        let sl_stmts = split_statements_for_engine(sqlite_sql, DbEngine::Sqlite);
        assert_eq!(sl_stmts.len(), 2);

        let formatted = format_sql("pragma table_info(`students`); replace into `students` values (1);");
        assert!(formatted.contains("PRAGMA"));
        assert!(formatted.contains("`students`"));
        assert!(formatted.contains("REPLACE"));
    }
}
