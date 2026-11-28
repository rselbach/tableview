use std::collections::VecDeque;

use sqlparser::dialect::{MySqlDialect, PostgreSqlDialect};
use sqlparser::keywords::Keyword;
use sqlparser::parser::{Parser, ParserError};
use sqlparser::tokenizer::{Token, TokenWithSpan, Whitespace, Word};

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
    PlainSingleQuote,
    EscapeSingleQuote,
    DoubleQuote,
    BacktickQuote,
    BracketQuote,
    LineComment,
    BlockComment(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EscapeContinuation {
    None,
    AfterClose,
    AfterNewline,
    PlainAfterClose,
    PlainAfterNewline,
}

pub(crate) fn is_sql_whitespace(engine: DbEngine, ch: char) -> bool {
    match engine {
        DbEngine::Postgres | DbEngine::Mysql => {
            matches!(ch, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c')
        }
        DbEngine::Sqlite => ch.is_ascii_whitespace(),
    }
}

/// Trims leading and trailing SQL whitespace recognized by `engine`, preserving non-SQL
/// Unicode whitespace (such as U+00A0) so scanners and database executors see identical text.
pub fn trim_sql_whitespace_for_engine(engine: DbEngine, s: &str) -> &str {
    s.trim_matches(|c: char| is_sql_whitespace(engine, c))
}

pub(crate) fn mysql_stmt_may_touch_sql_mode_text(stmt_sql: &str) -> bool {
    contains_ascii_ci(stmt_sql, "sql_mode")
        || contains_ascii_ci(stmt_sql, "execute")
        || contains_ascii_ci(stmt_sql, "call")
}

fn skip_mysql_ws_and_comments_idx(sql: &str, mut i: usize) -> usize {
    let bytes = sql.as_bytes();
    let len = bytes.len();
    while i < len {
        let ch = sql[i..].chars().next().unwrap_or('\0');
        if is_sql_whitespace(DbEngine::Mysql, ch) {
            i += ch.len_utf8();
        } else if is_dash_comment_start(DbEngine::Mysql, sql, i) {
            i += 2;
            while i < len && bytes[i] != b'\n' {
                i += char_len_at(sql, i);
            }
        } else if bytes[i] == b'#' {
            i += 1;
            while i < len && bytes[i] != b'\n' {
                i += char_len_at(sql, i);
            }
        } else if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            if sql[i..].starts_with("/*!") || sql[i..].starts_with("/*M!") {
                break;
            }
            i += 2;
            while i < len {
                if i + 1 < len && bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    i += 2;
                    break;
                }
                i += char_len_at(sql, i);
            }
        } else {
            break;
        }
    }
    i
}

pub(crate) fn strip_mariadb_set_statement_prefix(sql: &str, mode: MysqlQuoteMode) -> Option<&str> {
    let bytes = sql.as_bytes();
    let len = bytes.len();
    let mut i = skip_mysql_ws_and_comments_idx(sql, 0);
    let read_word = |pos: usize| -> Option<(&str, usize)> {
        let ch = sql[pos..].chars().next()?;
        if !is_ident_start_for_engine(DbEngine::Mysql, ch) {
            return None;
        }
        let mut end = pos + ch.len_utf8();
        while end < len {
            let nc = sql[end..].chars().next().unwrap_or('\0');
            if !is_ident_continue_for_engine(DbEngine::Mysql, nc) {
                break;
            }
            end += nc.len_utf8();
        }
        Some((&sql[pos..end], end))
    };

    let (w1, next1) = read_word(i)?;
    if !w1.eq_ignore_ascii_case("SET") {
        return None;
    }
    i = skip_mysql_ws_and_comments_idx(sql, next1);
    let (w2, next2) = read_word(i)?;
    if !w2.eq_ignore_ascii_case("STATEMENT") {
        return None;
    }
    i = next2;

    let allow_sq_bs = mode.allow_single_quote_backslash();
    let allow_dq_bs = mode.allow_double_quote_backslash();
    let allow_brackets = mode.allow_bracket_quotes();
    let mut paren_depth = 0usize;

    while i < len {
        let ch = sql[i..].chars().next().unwrap_or('\0');
        if is_sql_whitespace(DbEngine::Mysql, ch) {
            i += ch.len_utf8();
        } else if is_dash_comment_start(DbEngine::Mysql, sql, i) {
            i += 2;
            while i < len && bytes[i] != b'\n' {
                i += char_len_at(sql, i);
            }
        } else if bytes[i] == b'#' {
            i += 1;
            while i < len && bytes[i] != b'\n' {
                i += char_len_at(sql, i);
            }
        } else if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            while i < len {
                if i + 1 < len && bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    i += 2;
                    break;
                }
                i += char_len_at(sql, i);
            }
        } else if bytes[i] == b'\'' {
            i = skip_quoted_bytes(sql, i, b'\'', allow_sq_bs);
        } else if bytes[i] == b'"' {
            i = skip_quoted_bytes(sql, i, b'"', allow_dq_bs);
        } else if bytes[i] == b'`' {
            i = skip_quoted_bytes(sql, i, b'`', false);
        } else if allow_brackets && bytes[i] == b'[' {
            i = read_bracket_raw(sql, i, true).0;
        } else if bytes[i] == b'(' {
            paren_depth += 1;
            i += 1;
        } else if bytes[i] == b')' {
            paren_depth = paren_depth.saturating_sub(1);
            i += 1;
        } else if is_ident_start_for_engine(DbEngine::Mysql, ch) {
            let word_start = i;
            i += ch.len_utf8();
            while i < len {
                let nc = sql[i..].chars().next().unwrap_or('\0');
                if !is_ident_continue_for_engine(DbEngine::Mysql, nc) {
                    break;
                }
                i += nc.len_utf8();
            }
            if paren_depth == 0 && sql[word_start..i].eq_ignore_ascii_case("FOR") {
                let rest = trim_sql_whitespace_for_engine(DbEngine::Mysql, &sql[i..]);
                return (!rest.is_empty()).then_some(rest);
            }
        } else {
            i += ch.len_utf8();
        }
    }
    None
}

pub(crate) fn strip_all_mariadb_set_statement_prefixes(sql: &str, mode: MysqlQuoteMode) -> Option<&str> {
    let mut cur = strip_mariadb_set_statement_prefix(sql, mode)?;
    while let Some(next) = strip_mariadb_set_statement_prefix(cur, mode) {
        cur = next;
    }
    Some(cur)
}

fn is_non_ascii_ident_char(ch: char) -> bool {
    !ch.is_ascii() && !ch.is_whitespace() && ch != '\u{feff}'
}

fn is_ident_continue_for_engine(engine: DbEngine, ch: char) -> bool {
    match engine {
        // PG scan.l treats every non-ASCII byte (\200-\377) as an identifier character.
        DbEngine::Postgres => ch.is_ascii_alphanumeric() || ch == '_' || ch == '$' || !ch.is_ascii(),
        DbEngine::Mysql | DbEngine::Sqlite => {
            ch.is_alphanumeric() || ch == '_' || ch == '$' || is_non_ascii_ident_char(ch)
        }
    }
}

fn is_ident_start_for_engine(engine: DbEngine, ch: char) -> bool {
    match engine {
        DbEngine::Postgres => ch.is_ascii_alphabetic() || ch == '_' || !ch.is_ascii(),
        DbEngine::Mysql => {
            ch.is_alphabetic() || ch == '_' || ch == '$' || is_non_ascii_ident_char(ch)
        }
        DbEngine::Sqlite => ch.is_alphabetic() || ch == '_' || is_non_ascii_ident_char(ch),
    }
}

pub(crate) fn is_dash_comment_start(engine: DbEngine, sql: &str, i: usize) -> bool {
    let bytes = sql.as_bytes();
    if i + 1 >= bytes.len() || bytes[i] != b'-' || bytes[i + 1] != b'-' {
        return false;
    }
    if engine != DbEngine::Mysql {
        return true;
    }
    bytes
        .get(i + 2)
        .is_none_or(|&b| b.is_ascii_control() || b == b' ')
}

fn preceded_by_ident(engine: DbEngine, input: &str, idx: usize) -> bool {
    input[..idx]
        .chars()
        .next_back()
        .is_some_and(|ch| is_ident_continue_for_engine(engine, ch))
}

fn is_sqlite_trigger_header(words: &[String]) -> bool {
    let rest = match words {
        [a, b, c, rest @ ..] if a == "EXPLAIN" && b == "QUERY" && c == "PLAN" => rest,
        [a, rest @ ..] if a == "EXPLAIN" => rest,
        _ => words,
    };
    rest.first().is_some_and(|w| w == "CREATE")
        && (rest.get(1).is_some_and(|w| w == "TRIGGER")
            || (rest.get(1).is_some_and(|w| w == "TEMP" || w == "TEMPORARY")
                && rest.get(2).is_some_and(|w| w == "TRIGGER")))
}

fn is_pg_routine_header(words: &[String]) -> bool {
    match words {
        [a, b, ..] if a == "CREATE" && (b == "FUNCTION" || b == "PROCEDURE") => true,
        [a, b, c, d, ..]
            if a == "CREATE" && b == "OR" && c == "REPLACE" && (d == "FUNCTION" || d == "PROCEDURE") =>
        {
            true
        }
        _ => false,
    }
}

fn is_pg_rule_header(words: &[String]) -> bool {
    match words {
        [a, b, ..] if a == "CREATE" && b == "RULE" => true,
        [a, b, c, d, ..] if a == "CREATE" && b == "OR" && c == "REPLACE" && d == "RULE" => true,
        _ => false,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MysqlFlavor {
    Mysql,
    MariaDb,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MysqlServerVersion {
    pub flavor: MysqlFlavor,
    /// `major * 10_000 + minor * 100 + patch`, e.g. `8.4.0` => `80_400`, `11.4.2` => `110_402`.
    pub version_id: u32,
}

impl MysqlServerVersion {
    /// Parses a raw MySQL `VERSION()` or concise `"MySQL 8.4.0"` / `"MariaDB 11.4.2"` string.
    pub fn from_version_str(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return None;
        }
        let lower = trimmed.to_ascii_lowercase();
        let flavor = if lower.contains("mariadb") {
            MysqlFlavor::MariaDb
        } else {
            MysqlFlavor::Mysql
        };
        let after_label = trimmed
            .strip_prefix("MySQL ")
            .or_else(|| trimmed.strip_prefix("mysql "))
            .or_else(|| trimmed.strip_prefix("MariaDB "))
            .or_else(|| trimmed.strip_prefix("mariadb "))
            .unwrap_or(trimmed)
            .trim_start();
        let token = after_label.split_whitespace().next().unwrap_or(after_label);
        let token = if flavor == MysqlFlavor::MariaDb {
            token.strip_prefix("5.5.5-").unwrap_or(token)
        } else {
            token
        };
        let ver = token.split('-').next().unwrap_or(token).trim();
        let mut parts = ver.split('.');
        let major: u32 = parts.next()?.parse().ok()?;
        let minor: u32 = parts.next()?.parse().ok()?;
        let patch_str = parts.next().unwrap_or("0");
        let patch_digits: String = patch_str.chars().take_while(char::is_ascii_digit).collect();
        let patch: u32 = if patch_digits.is_empty() {
            0
        } else {
            patch_digits.parse().ok()?
        };
        if major >= 100 || minor >= 100 || patch >= 100 {
            return None;
        }
        Some(Self {
            flavor,
            version_id: major * 10_000 + minor * 100 + patch,
        })
    }

    pub(crate) const fn executes_comment(self, mariadb_only: bool, gate_version: Option<u32>) -> bool {
        if mariadb_only {
            return matches!(self.flavor, MysqlFlavor::MariaDb)
                && match gate_version {
                    None => true,
                    Some(v) => v <= self.version_id,
                };
        }
        match gate_version {
            None => true,
            Some(v) => match self.flavor {
                MysqlFlavor::Mysql => v <= self.version_id,
                MysqlFlavor::MariaDb => (v < 50_700 || v > 99_999) && v <= self.version_id,
            },
        }
    }

    pub(crate) const fn executes_comment_with_wsrep(
        self,
        mariadb_only: bool,
        gate_version: Option<u32>,
        mariadb_wsrep: bool,
    ) -> bool {
        if self.executes_comment(mariadb_only, gate_version) {
            return true;
        }
        mariadb_wsrep
            && matches!(self.flavor, MysqlFlavor::MariaDb)
            && matches!(gate_version, Some(99_997))
    }

    pub(crate) const fn version_lexer(self) -> MysqlVersionLexer {
        match self.flavor {
            MysqlFlavor::MariaDb => MysqlVersionLexer::MariaDb,
            MysqlFlavor::Mysql if self.version_id >= 80_100 => MysqlVersionLexer::Mysql6DigitWithWs,
            MysqlFlavor::Mysql => MysqlVersionLexer::Mysql5Digit,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MysqlVersionLexer {
    /// MySQL < 8.1.0 (e.g., 5.7, 8.0): consumes 5 digits iff at least 5 digits follow `/*!`.
    Mysql5Digit,
    /// MySQL >= 8.1.0 (e.g., 8.4, 9.x): consumes 6 digits iff followed by MySQL whitespace;
    /// otherwise consumes 5 digits iff at least 5 digits follow `/*!`.
    Mysql6DigitWithWs,
    /// MariaDB: consumes 6 digits iff at least 6 digits follow `/*!` or `/*M!`, else 5 digits.
    MariaDb,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MysqlExecCommentHeader {
    pub prefix_len: usize,
    pub mariadb_only: bool,
    pub gate_version: Option<u32>,
}

impl MysqlExecCommentHeader {
    pub(crate) const fn is_conditional(self) -> bool {
        self.mariadb_only || self.gate_version.is_some()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MysqlCommentPolicy {
    Server {
        version: MysqlServerVersion,
        mariadb_wsrep: bool,
    },
    UnconditionalOnly,
    UnwrapAll,
}

impl MysqlCommentPolicy {
    pub(crate) const fn server(version: MysqlServerVersion) -> Self {
        Self::Server {
            version,
            mariadb_wsrep: false,
        }
    }

    pub(crate) const fn server_wsrep(version: MysqlServerVersion) -> Self {
        Self::Server {
            version,
            mariadb_wsrep: true,
        }
    }

    pub(crate) const fn apply_to_mode(self, mode: MysqlQuoteMode) -> MysqlQuoteMode {
        match self {
            Self::Server { version, .. } => mode.with_server_version(Some(version)),
            Self::UnconditionalOnly | Self::UnwrapAll => mode,
        }
    }

    pub(crate) const fn should_unwrap(self, header: MysqlExecCommentHeader) -> bool {
        match self {
            Self::Server {
                version,
                mariadb_wsrep,
            } => version.executes_comment_with_wsrep(
                header.mariadb_only,
                header.gate_version,
                mariadb_wsrep,
            ),
            Self::UnconditionalOnly => !header.is_conditional(),
            Self::UnwrapAll => true,
        }
    }

    pub(crate) const fn unwrap_prefix_len(self, header: MysqlExecCommentHeader) -> usize {
        if let Self::Server {
            version,
            mariadb_wsrep: true,
        } = self
            && matches!(version.flavor, MysqlFlavor::MariaDb)
            && matches!(header.gate_version, Some(99_997))
            && !version.executes_comment(header.mariadb_only, header.gate_version)
        {
            return (if header.mariadb_only { 4 } else { 3 }) + 5;
        }
        header.prefix_len
    }

    pub(crate) const fn lexer(self) -> MysqlVersionLexer {
        match self {
            Self::Server { version, .. } => version.version_lexer(),
            Self::UnconditionalOnly => MysqlVersionLexer::Mysql6DigitWithWs,
            Self::UnwrapAll => MysqlVersionLexer::MariaDb,
        }
    }

    pub(crate) const fn inactive_comment_allows_nested(self, header: MysqlExecCommentHeader) -> bool {
        if header.gate_version.is_none() {
            return false;
        }
        match self {
            Self::Server { version, .. } => match version.flavor {
                MysqlFlavor::Mysql => !header.mariadb_only,
                MysqlFlavor::MariaDb => true,
            },
            Self::UnconditionalOnly => !header.mariadb_only,
            Self::UnwrapAll => false,
        }
    }
}

pub(crate) fn mysql_policies_for_known_server(
    sql: &str,
    sv: MysqlServerVersion,
) -> Vec<MysqlCommentPolicy> {
    let base = MysqlCommentPolicy::server(sv);
    if matches!(sv.flavor, MysqlFlavor::MariaDb) && sql.contains("99997") {
        vec![base, MysqlCommentPolicy::server_wsrep(sv)]
    } else {
        vec![base]
    }
}

/// Skips an inactive MySQL/MariaDB executable comment body starting after its header prefix
/// (`/*!...` or `/*M!...`), returning the byte index immediately after the closing `*/`
/// (or `sql.len()` if unclosed). When `allow_nested_level` is `true`, permits one nested
/// `/* ... */` block matching MySQL's `consume_comment(..., 1)` lexer rule.
pub(crate) fn skip_mysql_block_comment_body(
    sql: &str,
    body_start: usize,
    allow_nested_level: bool,
) -> usize {
    let bytes = sql.as_bytes();
    let len = bytes.len();
    let mut i = body_start;
    let mut in_nested = false;
    while i < len {
        if allow_nested_level && !in_nested && i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            in_nested = true;
            i += 2;
        } else if i + 1 < len && bytes[i] == b'*' && bytes[i + 1] == b'/' {
            i += 2;
            if in_nested {
                in_nested = false;
            } else {
                return i;
            }
        } else {
            i += char_len_at(sql, i);
        }
    }
    len
}

fn blank_sql_slice_preserving_newlines(slice: &str, out: &mut String) {
    for ch in slice.chars() {
        out.push(if ch == '\n' { '\n' } else { ' ' });
    }
}

pub(crate) fn parse_mysql_exec_comment_header(
    slice: &str,
    lexer: MysqlVersionLexer,
) -> Option<MysqlExecCommentHeader> {
    let (base, mariadb_only) = if slice.starts_with("/*!") {
        (3usize, false)
    } else if slice.starts_with("/*M!") {
        (4usize, true)
    } else {
        return None;
    };
    let digits = slice[base..]
        .bytes()
        .take_while(u8::is_ascii_digit)
        .count();
    let consumed = match lexer {
        MysqlVersionLexer::Mysql5Digit => {
            if digits >= 5 {
                5
            } else {
                0
            }
        }
        MysqlVersionLexer::Mysql6DigitWithWs => {
            if digits == 6
                && slice[base + 6..]
                    .chars()
                    .next()
                    .is_some_and(|c| is_sql_whitespace(DbEngine::Mysql, c))
            {
                6
            } else if digits >= 5 {
                5
            } else {
                0
            }
        }
        MysqlVersionLexer::MariaDb => {
            if digits >= 6 {
                6
            } else if digits == 5 {
                5
            } else {
                0
            }
        }
    };
    let gate_version = (consumed > 0)
        .then(|| slice[base..base + consumed].parse::<u32>().ok())
        .flatten();
    Some(MysqlExecCommentHeader {
        prefix_len: base + consumed,
        mariadb_only,
        gate_version,
    })
}

fn mysql_executable_comment_prefix_len(slice: &str) -> Option<usize> {
    parse_mysql_exec_comment_header(slice, MysqlVersionLexer::MariaDb).map(|h| h.prefix_len)
}

pub(crate) struct PlausibleMysqlPolicies {
    pub policies: Vec<MysqlCommentPolicy>,
    pub budget_exhausted: bool,
}

const MAX_PLAUSIBLE_VERSION_GATES: usize = 8;

struct VersionSpaceResult {
    visited: Vec<(MysqlServerVersion, bool)>,
    saw_exec_comment: bool,
    budget_exhausted: bool,
}

fn explore_version_space(
    sql: &str,
    mode: MysqlQuoteMode,
    seed: MysqlServerVersion,
    min_ver: u32,
    max_ver: u32,
) -> VersionSpaceResult {
    let bytes = sql.as_bytes();
    let len = bytes.len();
    let mut visited = Vec::new();
    let mut queue = VecDeque::from([(seed, false)]);
    let mut discovered_versions = vec![seed.version_id];
    let mut saw_wsrep_gate = false;
    let mut saw_exec_comment = false;
    let mut budget_exhausted = false;

    while let Some((sv, mariadb_wsrep)) = queue.pop_front() {
        visited.push((sv, mariadb_wsrep));
        let policy = MysqlCommentPolicy::Server {
            version: sv,
            mariadb_wsrep,
        };
        let space_mode = policy.apply_to_mode(mode);
        let allow_sq_backslash = space_mode.allow_single_quote_backslash();
        let allow_dq_backslash = space_mode.allow_double_quote_backslash();
        let allow_brackets = space_mode.allow_bracket_quotes();
        let lexer = policy.lexer();

        let mut i = 0usize;
        let mut state = ScanState::Normal;
        let mut mysql_exec_depth = 0usize;

        while i < len {
            match state {
                ScanState::Normal => {
                    if bytes[i] == b'\'' {
                        i += 1;
                        state = ScanState::SingleQuote;
                    } else if bytes[i] == b'"' {
                        i += 1;
                        state = ScanState::DoubleQuote;
                    } else if bytes[i] == b'`' {
                        i += 1;
                        state = ScanState::BacktickQuote;
                    } else if allow_brackets && bytes[i] == b'[' {
                        i += 1;
                        state = ScanState::BracketQuote;
                    } else if is_dash_comment_start(DbEngine::Mysql, sql, i) {
                        i += 2;
                        state = ScanState::LineComment;
                    } else if bytes[i] == b'#' {
                        i += 1;
                        state = ScanState::LineComment;
                    } else if mysql_exec_depth > 0 && i + 1 < len && bytes[i] == b'*' && bytes[i + 1] == b'/' {
                        i += 2;
                        mysql_exec_depth -= 1;
                    } else if let Some(header) = parse_mysql_exec_comment_header(&sql[i..], lexer) {
                        saw_exec_comment = true;
                        if matches!(sv.flavor, MysqlFlavor::MariaDb)
                            && header.gate_version == Some(99_997)
                            && !sv.executes_comment(header.mariadb_only, header.gate_version)
                            && !saw_wsrep_gate
                        {
                            saw_wsrep_gate = true;
                            for &ver in &discovered_versions {
                                queue.push_back((
                                    MysqlServerVersion {
                                        flavor: sv.flavor,
                                        version_id: ver,
                                    },
                                    true,
                                ));
                            }
                        }
                        let applicable = match sv.flavor {
                            MysqlFlavor::Mysql => !header.mariadb_only,
                            MysqlFlavor::MariaDb => {
                                header.mariadb_only
                                    || !header.gate_version.is_some_and(|v| (50_700..=99_999).contains(&v))
                            }
                        };
                        if applicable
                            && let Some(v) = header.gate_version
                            && v > min_ver
                            && v <= max_ver
                            && !discovered_versions.contains(&v)
                        {
                            if discovered_versions.len() <= MAX_PLAUSIBLE_VERSION_GATES {
                                discovered_versions.push(v);
                                let next_sv = MysqlServerVersion {
                                    flavor: sv.flavor,
                                    version_id: v,
                                };
                                queue.push_back((next_sv, false));
                                if saw_wsrep_gate {
                                    queue.push_back((next_sv, true));
                                }
                            } else {
                                budget_exhausted = true;
                            }
                        }
                        if policy.should_unwrap(header) {
                            i += policy.unwrap_prefix_len(header);
                            mysql_exec_depth += 1;
                        } else {
                            i = skip_mysql_block_comment_body(
                                sql,
                                i + header.prefix_len,
                                policy.inactive_comment_allows_nested(header),
                            );
                        }
                    } else if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
                        i += 2;
                        state = ScanState::BlockComment(1);
                    } else {
                        i += char_len_at(sql, i);
                    }
                }
                ScanState::SingleQuote => {
                    if allow_sq_backslash && bytes[i] == b'\\' {
                        i += 1;
                        if i < len {
                            i += char_len_at(sql, i);
                        }
                    } else if bytes[i] == b'\'' {
                        i += 1;
                        if i < len && bytes[i] == b'\'' {
                            i += 1;
                        } else {
                            state = ScanState::Normal;
                        }
                    } else {
                        i += char_len_at(sql, i);
                    }
                }
                ScanState::DoubleQuote => {
                    if allow_dq_backslash && bytes[i] == b'\\' {
                        i += 1;
                        if i < len {
                            i += char_len_at(sql, i);
                        }
                    } else if bytes[i] == b'"' {
                        i += 1;
                        if i < len && bytes[i] == b'"' {
                            i += 1;
                        } else {
                            state = ScanState::Normal;
                        }
                    } else {
                        i += char_len_at(sql, i);
                    }
                }
                ScanState::BacktickQuote => {
                    if bytes[i] == b'`' {
                        i += 1;
                        if i < len && bytes[i] == b'`' {
                            i += 1;
                        } else {
                            state = ScanState::Normal;
                        }
                    } else {
                        i += char_len_at(sql, i);
                    }
                }
                ScanState::BracketQuote => {
                    if bytes[i] == b']' {
                        i += 1;
                        if i < len && bytes[i] == b']' {
                            i += 1;
                        } else {
                            state = ScanState::Normal;
                        }
                    } else {
                        i += char_len_at(sql, i);
                    }
                }
                ScanState::LineComment => {
                    if bytes[i] == b'\n' {
                        state = ScanState::Normal;
                    }
                    i += char_len_at(sql, i);
                }
                ScanState::BlockComment(_) => {
                    if i + 1 < len && bytes[i] == b'*' && bytes[i + 1] == b'/' {
                        i += 2;
                        state = ScanState::Normal;
                    } else {
                        i += char_len_at(sql, i);
                    }
                }
                ScanState::EscapeSingleQuote | ScanState::PlainSingleQuote => {
                    i += char_len_at(sql, i);
                }
            }
        }
    }

    VersionSpaceResult {
        visited,
        saw_exec_comment,
        budget_exhausted,
    }
}

pub(crate) fn plausible_mysql_comment_policies(
    sql: &str,
    mode: MysqlQuoteMode,
    known_server: Option<MysqlServerVersion>,
) -> PlausibleMysqlPolicies {
    if !sql.contains("/*!") && !sql.contains("/*M!") {
        return PlausibleMysqlPolicies {
            policies: known_server.map_or_else(
                || vec![MysqlCommentPolicy::UnconditionalOnly],
                |sv| mysql_policies_for_known_server(sql, sv),
            ),
            budget_exhausted: false,
        };
    }

    let mut s1 = explore_version_space(
        sql,
        mode,
        MysqlServerVersion {
            flavor: MysqlFlavor::Mysql,
            version_id: 0,
        },
        0,
        80_099,
    );
    let mut s2 = explore_version_space(
        sql,
        mode,
        MysqlServerVersion {
            flavor: MysqlFlavor::Mysql,
            version_id: 80_100,
        },
        80_100,
        999_999,
    );
    let mut s3 = explore_version_space(
        sql,
        mode,
        MysqlServerVersion {
            flavor: MysqlFlavor::MariaDb,
            version_id: 0,
        },
        0,
        999_999,
    );

    if !s1.saw_exec_comment && !s2.saw_exec_comment && !s3.saw_exec_comment {
        return PlausibleMysqlPolicies {
            policies: known_server.map_or_else(
                || vec![MysqlCommentPolicy::UnconditionalOnly],
                |sv| mysql_policies_for_known_server(sql, sv),
            ),
            budget_exhausted: false,
        };
    }

    s1.visited
        .sort_unstable_by_key(|(sv, wsrep)| (sv.version_id, *wsrep));
    s2.visited
        .sort_unstable_by_key(|(sv, wsrep)| (sv.version_id, *wsrep));
    s3.visited
        .sort_unstable_by_key(|(sv, wsrep)| (sv.version_id, *wsrep));

    let mut out = Vec::new();
    let mut push_unique = |policy: MysqlCommentPolicy| {
        if !out.contains(&policy) {
            out.push(policy);
        }
    };
    if let Some(sv) = known_server {
        for policy in mysql_policies_for_known_server(sql, sv) {
            push_unique(policy);
        }
    }
    for (version, mariadb_wsrep) in s1.visited.into_iter().chain(s2.visited).chain(s3.visited) {
        push_unique(MysqlCommentPolicy::Server {
            version,
            mariadb_wsrep,
        });
    }
    PlausibleMysqlPolicies {
        policies: out,
        budget_exhausted: s1.budget_exhausted || s2.budget_exhausted || s3.budget_exhausted,
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct MysqlQuoteMode {
    pub ansi_quotes: bool,
    pub no_backslash_escapes: bool,
    pub mssql_quotes: bool,
    pub server_version: Option<MysqlServerVersion>,
}

impl MysqlQuoteMode {
    pub const DEFAULT: Self = Self {
        ansi_quotes: false,
        no_backslash_escapes: false,
        mssql_quotes: false,
        server_version: None,
    };
    pub const ANSI_QUOTES: Self = Self {
        ansi_quotes: true,
        no_backslash_escapes: false,
        mssql_quotes: false,
        server_version: None,
    };
    pub const NO_BACKSLASH_ESCAPES: Self = Self {
        ansi_quotes: false,
        no_backslash_escapes: true,
        mssql_quotes: false,
        server_version: None,
    };
    pub const ANSI_AND_NO_BACKSLASH: Self = Self {
        ansi_quotes: true,
        no_backslash_escapes: true,
        mssql_quotes: false,
        server_version: None,
    };
    pub const MSSQL: Self = Self {
        ansi_quotes: true,
        no_backslash_escapes: false,
        mssql_quotes: true,
        server_version: None,
    };
    pub const MSSQL_AND_NO_BACKSLASH: Self = Self {
        ansi_quotes: true,
        no_backslash_escapes: true,
        mssql_quotes: true,
        server_version: None,
    };

    /// Attaches an optional MySQL/MariaDB server version for conditional comment resolution.
    pub const fn with_server_version(mut self, server_version: Option<MysqlServerVersion>) -> Self {
        self.server_version = server_version;
        self
    }

    pub(crate) const fn quote_flags_eq(self, other: Self) -> bool {
        self.ansi_quotes == other.ansi_quotes
            && self.no_backslash_escapes == other.no_backslash_escapes
            && self.mssql_quotes == other.mssql_quotes
    }

    /// Parses MySQL's `@@SESSION.sql_mode` string for quoting-relevant flags.
    pub fn from_sql_mode(sql_mode: &str) -> Self {
        let mut mode = Self::DEFAULT;
        for token in sql_mode.split(',').map(str::trim) {
            if token.eq_ignore_ascii_case("ANSI_QUOTES")
                || token.eq_ignore_ascii_case("ANSI")
                || token.eq_ignore_ascii_case("MSSQL")
                || token.eq_ignore_ascii_case("POSTGRESQL")
                || token.eq_ignore_ascii_case("ORACLE")
                || token.eq_ignore_ascii_case("DB2")
                || token.eq_ignore_ascii_case("MAXDB")
            {
                mode.ansi_quotes = true;
            }
            if token.eq_ignore_ascii_case("MSSQL") {
                mode.mssql_quotes = true;
            }
            if token.eq_ignore_ascii_case("NO_BACKSLASH_ESCAPES") {
                mode.no_backslash_escapes = true;
            }
        }
        mode
    }

    pub(crate) const fn to_bits(self) -> u8 {
        (self.ansi_quotes as u8)
            | ((self.no_backslash_escapes as u8) << 1)
            | ((self.mssql_quotes as u8) << 2)
    }

    pub(crate) const fn from_bits(bits: u8) -> Self {
        Self {
            ansi_quotes: (bits & 1) != 0,
            no_backslash_escapes: (bits & 2) != 0,
            mssql_quotes: (bits & 4) != 0,
            server_version: None,
        }
    }

    pub(crate) fn as_sql_mode_string(self) -> &'static str {
        match (self.ansi_quotes, self.no_backslash_escapes, self.mssql_quotes) {
            (false, false, _) => "",
            (true, false, false) => "ANSI_QUOTES",
            (false, true, _) => "NO_BACKSLASH_ESCAPES",
            (true, true, false) => "ANSI_QUOTES,NO_BACKSLASH_ESCAPES",
            (true, false, true) => "ANSI_QUOTES,MSSQL",
            (true, true, true) => "ANSI_QUOTES,NO_BACKSLASH_ESCAPES,MSSQL",
        }
    }

    pub(crate) const fn allow_single_quote_backslash(self) -> bool {
        !self.no_backslash_escapes
    }

    pub(crate) const fn allow_double_quote_backslash(self) -> bool {
        !self.ansi_quotes && !self.no_backslash_escapes
    }

    pub(crate) const fn allow_bracket_quotes(self) -> bool {
        self.mssql_quotes
            && !matches!(
                self.server_version,
                Some(sv) if matches!(sv.flavor, MysqlFlavor::Mysql)
            )
    }
}

pub(crate) fn contains_ascii_ci(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    haystack
        .as_bytes()
        .windows(needle.len())
        .any(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
}

pub(crate) fn unwrap_mysql_executable_comments_with_mode(
    sql: &str,
    mode: MysqlQuoteMode,
) -> (String, bool) {
    let policy = mode
        .server_version
        .map_or(MysqlCommentPolicy::UnwrapAll, MysqlCommentPolicy::server);
    let (out, ended_in_quote, _) = unwrap_mysql_executable_comments_with_policy(sql, mode, policy);
    (out, ended_in_quote)
}

pub(crate) fn unwrap_mysql_executable_comments_with_policy(
    sql: &str,
    mode: MysqlQuoteMode,
    policy: MysqlCommentPolicy,
) -> (String, bool, bool) {
    let bytes = sql.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len);
    let mut i = 0usize;
    let mut state = ScanState::Normal;
    let mut mysql_exec_depth = 0usize;
    let mut saw_conditional = false;
    let effective_mode = policy.apply_to_mode(mode);
    let allow_sq_backslash = effective_mode.allow_single_quote_backslash();
    let allow_dq_backslash = effective_mode.allow_double_quote_backslash();
    let allow_brackets = effective_mode.allow_bracket_quotes();
    let lexer = policy.lexer();

    while i < len {
        match state {
            ScanState::Normal => {
                if bytes[i] == b'\'' {
                    out.push('\'');
                    i += 1;
                    state = ScanState::SingleQuote;
                } else if bytes[i] == b'"' {
                    out.push('"');
                    i += 1;
                    state = ScanState::DoubleQuote;
                } else if bytes[i] == b'`' {
                    out.push('`');
                    i += 1;
                    state = ScanState::BacktickQuote;
                } else if allow_brackets && bytes[i] == b'[' {
                    out.push('[');
                    i += 1;
                    state = ScanState::BracketQuote;
                } else if is_dash_comment_start(DbEngine::Mysql, sql, i) {
                    out.push_str("--");
                    i += 2;
                    state = ScanState::LineComment;
                } else if bytes[i] == b'#' {
                    out.push('#');
                    i += 1;
                    state = ScanState::LineComment;
                } else if mysql_exec_depth > 0 && i + 1 < len && bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    out.push_str("  ");
                    i += 2;
                    mysql_exec_depth -= 1;
                } else if let Some(header) = parse_mysql_exec_comment_header(&sql[i..], lexer) {
                    if header.is_conditional() {
                        saw_conditional = true;
                    }
                    if policy.should_unwrap(header) {
                        let skip = policy.unwrap_prefix_len(header);
                        for _ in 0..skip {
                            out.push(' ');
                        }
                        i += skip;
                        mysql_exec_depth += 1;
                    } else {
                        let end = skip_mysql_block_comment_body(
                            sql,
                            i + header.prefix_len,
                            policy.inactive_comment_allows_nested(header),
                        );
                        blank_sql_slice_preserving_newlines(&sql[i..end], &mut out);
                        i = end;
                    }
                } else if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
                    out.push_str("/*");
                    i += 2;
                    state = ScanState::BlockComment(1);
                } else {
                    let ch = sql[i..].chars().next().unwrap_or('\0');
                    out.push(ch);
                    i += ch.len_utf8();
                }
            }
            ScanState::SingleQuote => {
                if allow_sq_backslash && bytes[i] == b'\\' {
                    out.push('\\');
                    i += 1;
                    if i < len {
                        let ch = sql[i..].chars().next().unwrap_or('\0');
                        out.push(ch);
                        i += ch.len_utf8();
                    }
                } else if bytes[i] == b'\'' {
                    out.push('\'');
                    i += 1;
                    if i < len && bytes[i] == b'\'' {
                        out.push('\'');
                        i += 1;
                    } else {
                        state = ScanState::Normal;
                    }
                } else {
                    let ch = sql[i..].chars().next().unwrap_or('\0');
                    out.push(ch);
                    i += ch.len_utf8();
                }
            }
            ScanState::DoubleQuote => {
                if allow_dq_backslash && bytes[i] == b'\\' {
                    out.push('\\');
                    i += 1;
                    if i < len {
                        let ch = sql[i..].chars().next().unwrap_or('\0');
                        out.push(ch);
                        i += ch.len_utf8();
                    }
                } else if bytes[i] == b'"' {
                    out.push('"');
                    i += 1;
                    if i < len && bytes[i] == b'"' {
                        out.push('"');
                        i += 1;
                    } else {
                        state = ScanState::Normal;
                    }
                } else {
                    let ch = sql[i..].chars().next().unwrap_or('\0');
                    out.push(ch);
                    i += ch.len_utf8();
                }
            }
            ScanState::BacktickQuote => {
                if bytes[i] == b'`' {
                    out.push('`');
                    i += 1;
                    if i < len && bytes[i] == b'`' {
                        out.push('`');
                        i += 1;
                    } else {
                        state = ScanState::Normal;
                    }
                } else {
                    let ch = sql[i..].chars().next().unwrap_or('\0');
                    out.push(ch);
                    i += ch.len_utf8();
                }
            }
            ScanState::LineComment => {
                let ch = sql[i..].chars().next().unwrap_or('\0');
                out.push(ch);
                i += ch.len_utf8();
                if ch == '\n' {
                    state = ScanState::Normal;
                }
            }
            ScanState::BlockComment(_) => {
                if i + 1 < len && bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    out.push_str("*/");
                    i += 2;
                    state = ScanState::Normal;
                } else {
                    let ch = sql[i..].chars().next().unwrap_or('\0');
                    out.push(ch);
                    i += ch.len_utf8();
                }
            }
            ScanState::BracketQuote => {
                if bytes[i] == b']' {
                    out.push(']');
                    i += 1;
                    if i < len && bytes[i] == b']' {
                        out.push(']');
                        i += 1;
                    } else {
                        state = ScanState::Normal;
                    }
                } else {
                    let ch = sql[i..].chars().next().unwrap_or('\0');
                    out.push(ch);
                    i += ch.len_utf8();
                }
            }
            ScanState::EscapeSingleQuote | ScanState::PlainSingleQuote => {
                let ch = sql[i..].chars().next().unwrap_or('\0');
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }

    (
        out,
        matches!(
            state,
            ScanState::SingleQuote
                | ScanState::PlainSingleQuote
                | ScanState::DoubleQuote
                | ScanState::BacktickQuote
                | ScanState::BracketQuote
        ),
        saw_conditional,
    )
}

pub(crate) fn is_closed_mysql_string_literal(sql: &str, quote_pos: usize) -> bool {
    let bytes = sql.as_bytes();
    quote_pos < bytes.len()
        && matches!(bytes[quote_pos], b'\'' | b'"')
        && read_quoted_raw(sql, quote_pos, bytes[quote_pos], true).2
}

pub(crate) fn mysql_normalize_for_parser(sql: &str, mode: MysqlQuoteMode) -> String {
    let bytes = sql.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len);
    let mut i = 0usize;
    let mut state = ScanState::Normal;

    while i < len {
        match state {
            ScanState::Normal => {
                if bytes[i] == b'\'' {
                    out.push('\'');
                    i += 1;
                    state = ScanState::SingleQuote;
                } else if bytes[i] == b'"' {
                    out.push(if mode.ansi_quotes { '`' } else { '"' });
                    i += 1;
                    state = ScanState::DoubleQuote;
                } else if bytes[i] == b'`' {
                    out.push('`');
                    i += 1;
                    state = ScanState::BacktickQuote;
                } else if mode.allow_bracket_quotes() && bytes[i] == b'[' {
                    let (next_i, ident, closed) = read_bracket_raw(sql, i, true);
                    if closed {
                        out.push('`');
                        for c in ident.chars() {
                            if c == '`' {
                                out.push_str("``");
                            } else {
                                out.push(c);
                            }
                        }
                        out.push('`');
                        i = next_i;
                    } else {
                        out.push_str(&sql[i..]);
                        i = len;
                    }
                } else if is_dash_comment_start(DbEngine::Mysql, sql, i) {
                    out.push_str("  ");
                    i += 2;
                    state = ScanState::LineComment;
                } else if i + 1 < len && bytes[i] == b'-' && bytes[i + 1] == b'-' {
                    // Split non-comment `--` (e.g., `--\u{00a0}`, `---\u{00a0}`, `--1`) into `- `
                    // advancing by 1 byte so sqlparser never treats Unicode whitespace after `--`
                    // as a line comment while `--- ` still starts a comment at the second `-`.
                    out.push_str("- ");
                    i += 1;
                } else if bytes[i] == b'#' {
                    out.push(' ');
                    i += 1;
                    state = ScanState::LineComment;
                } else if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
                    out.push_str("/*");
                    i += 2;
                    state = ScanState::BlockComment(1);
                } else {
                    let ch = sql[i..].chars().next().unwrap_or('\0');
                    out.push(ch);
                    i += ch.len_utf8();
                }
            }
            ScanState::SingleQuote => {
                if bytes[i] == b'\\' {
                    if mode.no_backslash_escapes {
                        out.push_str("\\\\");
                        i += 1;
                    } else {
                        out.push('\\');
                        i += 1;
                        if i < len {
                            let ch = sql[i..].chars().next().unwrap_or('\0');
                            out.push(ch);
                            i += ch.len_utf8();
                        }
                    }
                } else if bytes[i] == b'\'' {
                    out.push('\'');
                    i += 1;
                    if i < len && bytes[i] == b'\'' {
                        out.push('\'');
                        i += 1;
                    } else {
                        state = ScanState::Normal;
                    }
                } else {
                    let ch = sql[i..].chars().next().unwrap_or('\0');
                    out.push(ch);
                    i += ch.len_utf8();
                }
            }
            ScanState::DoubleQuote => {
                if mode.ansi_quotes {
                    if bytes[i] == b'"' {
                        if i + 1 < len && bytes[i + 1] == b'"' {
                            out.push('"');
                            i += 2;
                        } else {
                            out.push('`');
                            i += 1;
                            state = ScanState::Normal;
                        }
                    } else if bytes[i] == b'`' {
                        out.push_str("``");
                        i += 1;
                    } else {
                        let ch = sql[i..].chars().next().unwrap_or('\0');
                        out.push(ch);
                        i += ch.len_utf8();
                    }
                } else if bytes[i] == b'\\' {
                    if mode.no_backslash_escapes {
                        out.push_str("\\\\");
                        i += 1;
                    } else {
                        out.push('\\');
                        i += 1;
                        if i < len {
                            let ch = sql[i..].chars().next().unwrap_or('\0');
                            out.push(ch);
                            i += ch.len_utf8();
                        }
                    }
                } else if bytes[i] == b'"' {
                    out.push('"');
                    i += 1;
                    if i < len && bytes[i] == b'"' {
                        out.push('"');
                        i += 1;
                    } else {
                        state = ScanState::Normal;
                    }
                } else {
                    let ch = sql[i..].chars().next().unwrap_or('\0');
                    out.push(ch);
                    i += ch.len_utf8();
                }
            }
            ScanState::BacktickQuote => {
                if bytes[i] == b'`' {
                    out.push('`');
                    i += 1;
                    if i < len && bytes[i] == b'`' {
                        out.push('`');
                        i += 1;
                    } else {
                        state = ScanState::Normal;
                    }
                } else {
                    let ch = sql[i..].chars().next().unwrap_or('\0');
                    out.push(ch);
                    i += ch.len_utf8();
                }
            }
            ScanState::LineComment => {
                let ch = sql[i..].chars().next().unwrap_or('\0');
                if ch == '\n' {
                    out.push('\n');
                    state = ScanState::Normal;
                } else {
                    out.push(' ');
                }
                i += ch.len_utf8();
            }
            ScanState::BlockComment(_) => {
                if i + 1 < len && bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    out.push_str("*/");
                    i += 2;
                    state = ScanState::Normal;
                } else {
                    let ch = sql[i..].chars().next().unwrap_or('\0');
                    out.push(ch);
                    i += ch.len_utf8();
                }
            }
            ScanState::EscapeSingleQuote | ScanState::PlainSingleQuote | ScanState::BracketQuote => {
                let ch = sql[i..].chars().next().unwrap_or('\0');
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }

    out
}

pub(crate) const MAX_TRACKED_PG_SAVEPOINTS: usize = 64;

/// Live PostgreSQL transaction block state tracked across statements.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PostgresTxState {
    Idle,
    InTransaction,
    Aborted,
}

/// Snapshot of PostgreSQL `standard_conforming_strings` state at a named `SAVEPOINT`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PostgresSavepointMode {
    pub name: String,
    pub active_standard_conforming_strings: Option<bool>,
    pub session_standard_conforming_strings: Option<bool>,
}

/// Server encoding classification used when tracking PostgreSQL savepoint identifier truncation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PostgresServerEncoding {
    /// UTF-8 (`UTF8` / `UNICODE`): `truncate_identifier` truncates at UTF-8 char boundaries up to `max_identifier_length` bytes.
    Utf8,
    /// Single-byte encodings (`LATIN*`, `ISO_8859_*`, `KOI8*`, `WIN*`): 1 character = 1 byte on the server.
    SingleByte,
    /// Other multi-byte or ASCII-passthrough encodings (`EUC_*`, `SQL_ASCII`, `MULE_INTERNAL`): non-ASCII truncation is treated as untracked.
    Other,
}

impl PostgresServerEncoding {
    /// Classifies a PostgreSQL `server_encoding` setting string.
    #[must_use]
    pub fn from_pg_name(name: &str) -> Self {
        let upper = name.trim().to_ascii_uppercase();
        match upper.as_str() {
            "UTF8" | "UNICODE" => Self::Utf8,
            "LATIN1" | "LATIN2" | "LATIN3" | "LATIN4" | "LATIN5" | "LATIN6" | "LATIN7"
            | "LATIN8" | "LATIN9" | "LATIN10" | "ISO_8859_5" | "ISO_8859_6"
            | "ISO_8859_7" | "ISO_8859_8" | "KOI8R" | "KOI8U" | "WIN866" | "WIN874"
            | "WIN1250" | "WIN1251" | "WIN1252" | "WIN1253" | "WIN1254" | "WIN1255"
            | "WIN1256" | "WIN1257" | "WIN1258" => Self::SingleByte,
            _ => Self::Other,
        }
    }
}

pub(crate) const PG_DEFAULT_MAX_IDENT_BYTES: usize = 63;

/// Live PostgreSQL session string-escaping and transaction state tracked across queries.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PostgresSessionMode {
    pub standard_conforming_strings: bool,
    pub reset_standard_conforming_strings: Option<bool>,
    pub tx_state: Option<PostgresTxState>,
    pub session_standard_conforming_strings: Option<bool>,
    pub pre_tx_standard_conforming_strings: Option<bool>,
    pub savepoints: Vec<PostgresSavepointMode>,
    pub has_untracked_savepoints: bool,
    pub max_identifier_length: usize,
    pub server_encoding: Option<PostgresServerEncoding>,
}

fn truncate_pg_quoted_savepoint_name(
    raw: &str,
    max_ident_bytes: usize,
    server_encoding: Option<PostgresServerEncoding>,
) -> Option<String> {
    let limit = max_ident_bytes.max(1);
    if raw.len() >= limit && raw.as_bytes()[..limit].is_ascii() {
        return Some(raw[..limit].to_owned());
    }
    if raw.is_ascii() || server_encoding == Some(PostgresServerEncoding::Utf8) {
        let end = clamp_char_boundary(raw, limit);
        Some(raw[..end].to_owned())
    } else if server_encoding == Some(PostgresServerEncoding::SingleByte) {
        Some(raw.chars().take(limit).collect())
    } else {
        None
    }
}

impl PostgresSessionMode {
    /// Creates a [`PostgresSessionMode`] outside a transaction with an active `standard_conforming_strings` setting.
    pub const fn new(standard_conforming_strings: bool) -> Self {
        Self {
            standard_conforming_strings,
            reset_standard_conforming_strings: None,
            tx_state: Some(PostgresTxState::Idle),
            session_standard_conforming_strings: Some(standard_conforming_strings),
            pre_tx_standard_conforming_strings: Some(standard_conforming_strings),
            savepoints: Vec::new(),
            has_untracked_savepoints: false,
            max_identifier_length: PG_DEFAULT_MAX_IDENT_BYTES,
            server_encoding: None,
        }
    }

    /// Creates a [`PostgresSessionMode`] inside an active or aborted transaction with unknown
    /// pre-transaction state and possibly untracked prior savepoints.
    pub const fn in_transaction(
        standard_conforming_strings: bool,
        tx_state: PostgresTxState,
    ) -> Self {
        let is_idle = matches!(tx_state, PostgresTxState::Idle);
        Self {
            standard_conforming_strings,
            reset_standard_conforming_strings: None,
            tx_state: Some(tx_state),
            session_standard_conforming_strings: if is_idle {
                Some(standard_conforming_strings)
            } else {
                None
            },
            pre_tx_standard_conforming_strings: if is_idle {
                Some(standard_conforming_strings)
            } else {
                None
            },
            savepoints: Vec::new(),
            has_untracked_savepoints: !is_idle,
            max_identifier_length: PG_DEFAULT_MAX_IDENT_BYTES,
            server_encoding: None,
        }
    }

    /// Sets the PostgreSQL `RESET` default value (`pg_settings.reset_val`) for `standard_conforming_strings`.
    pub fn with_reset_val(mut self, reset_val: Option<bool>) -> Self {
        self.reset_standard_conforming_strings = reset_val;
        self
    }

    /// Sets the PostgreSQL identifier truncation byte limit (`max_identifier_length`).
    pub fn with_max_identifier_length(mut self, max_identifier_length: usize) -> Self {
        self.max_identifier_length = max_identifier_length.max(1);
        self
    }

    /// Sets the PostgreSQL server encoding classification used for savepoint truncation rules.
    pub fn with_server_encoding(mut self, server_encoding: Option<PostgresServerEncoding>) -> Self {
        self.server_encoding = server_encoding;
        self
    }

    /// Sets PostgreSQL's current transaction block state.
    pub fn with_tx_state(mut self, tx_state: Option<PostgresTxState>) -> Self {
        self.tx_state = tx_state;
        if matches!(tx_state, Some(PostgresTxState::Idle)) {
            self.savepoints.clear();
            self.has_untracked_savepoints = false;
            self.session_standard_conforming_strings = Some(self.standard_conforming_strings);
            self.pre_tx_standard_conforming_strings = Some(self.standard_conforming_strings);
        } else {
            self.savepoints.clear();
            self.has_untracked_savepoints = true;
            self.session_standard_conforming_strings = None;
            self.pre_tx_standard_conforming_strings = None;
        }
        self
    }

    /// Sets whether the connection is currently inside an explicit transaction block.
    pub fn with_in_transaction(self, in_tx: Option<bool>) -> Self {
        let tx_state = match in_tx {
            Some(true) => Some(PostgresTxState::InTransaction),
            Some(false) => Some(PostgresTxState::Idle),
            None => None,
        };
        self.with_tx_state(tx_state)
    }

    /// Appends a tracked savepoint snapshot to this session mode.
    pub fn with_savepoint(
        mut self,
        name: impl Into<String>,
        active_scs: Option<bool>,
        session_scs: Option<bool>,
    ) -> Self {
        if self.savepoints.len() >= MAX_TRACKED_PG_SAVEPOINTS {
            self.savepoints.clear();
            self.has_untracked_savepoints = true;
        }
        let raw_name = name.into();
        if let Some(truncated) = truncate_pg_quoted_savepoint_name(
            &raw_name,
            self.max_identifier_length,
            self.server_encoding,
        ) {
            self.savepoints.push(PostgresSavepointMode {
                name: truncated,
                active_standard_conforming_strings: active_scs,
                session_standard_conforming_strings: session_scs,
            });
        } else {
            self.savepoints.clear();
            self.has_untracked_savepoints = true;
        }
        self
    }
}

/// Live session dialect configuration used by statement splitting, safety inspection, and history filtering.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct SqlSessionMode {
    pub mysql: Option<MysqlQuoteMode>,
    pub postgres: Option<PostgresSessionMode>,
}

impl SqlSessionMode {
    /// Creates a [`SqlSessionMode`] carrying an optional live MySQL [`MysqlQuoteMode`].
    pub const fn mysql(mysql: Option<MysqlQuoteMode>) -> Self {
        Self {
            mysql,
            postgres: None,
        }
    }

    /// Creates a [`SqlSessionMode`] carrying PostgreSQL's `standard_conforming_strings` setting.
    pub const fn postgres(standard_conforming_strings: bool) -> Self {
        Self {
            mysql: None,
            postgres: Some(PostgresSessionMode::new(standard_conforming_strings)),
        }
    }

    /// Creates a [`SqlSessionMode`] carrying an optional live [`PostgresSessionMode`].
    pub const fn postgres_mode(postgres: Option<PostgresSessionMode>) -> Self {
        Self {
            mysql: None,
            postgres,
        }
    }

    /// Returns PostgreSQL's `standard_conforming_strings` mode (defaulting to `true` when unset).
    pub const fn pg_standard_conforming_strings(&self) -> bool {
        match &self.postgres {
            Some(mode) => mode.standard_conforming_strings,
            None => true,
        }
    }

    /// Returns PostgreSQL's tracked [`PostgresSessionMode`] (defaulting to `scs = true` outside tx when unset).
    pub fn pg_session_mode(&self) -> PostgresSessionMode {
        match &self.postgres {
            Some(mode) => mode.clone(),
            None => PostgresSessionMode::new(true),
        }
    }
}

fn is_pg_select_like_toks(toks: &[CredTok]) -> bool {
    let mut trimmed = toks;
    while matches!(trimmed.last(), Some(CredTok::Punct(';'))) {
        trimmed = &trimmed[..trimmed.len() - 1];
    }
    if trimmed.iter().any(|tok| matches!(tok, CredTok::Punct(';'))) {
        return false;
    }
    let mut idx = 0usize;
    while matches!(trimmed.get(idx), Some(CredTok::Punct('('))) {
        idx += 1;
    }
    matches!(
        trimmed.get(idx),
        Some(CredTok::Word(w))
            if matches!(w.as_str(), "SELECT" | "WITH" | "VALUES" | "TABLE")
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PgScsChange {
    SetSession(bool),
    SetLocal(bool),
    ResetSession,
    ResetLocal,
    DiscardAll,
    BeginTx,
    CommitTx { chain: bool },
    PrepareTx,
    RollbackTx { chain: bool },
    Savepoint(Option<String>),
    ReleaseSavepoint(Option<String>),
    RollbackToSavepoint(Option<String>),
    ReloadConf,
    UncertainSetting,
    Uncertain,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PgScsTracker {
    pub(crate) active: Option<bool>,
    session: Option<bool>,
    reset_val: Option<bool>,
    pub(crate) tx_state: Option<PostgresTxState>,
    pre_tx: Option<bool>,
    savepoints: Vec<PostgresSavepointMode>,
    has_untracked_savepoints: bool,
    max_ident_bytes: usize,
    server_encoding: Option<PostgresServerEncoding>,
    pub(crate) saw_uncertain: bool,
}

impl PgScsTracker {
    pub(crate) fn from_mode(mode: &PostgresSessionMode) -> Self {
        Self {
            active: Some(mode.standard_conforming_strings),
            session: mode.session_standard_conforming_strings,
            reset_val: mode.reset_standard_conforming_strings,
            tx_state: mode.tx_state,
            pre_tx: mode.pre_tx_standard_conforming_strings,
            savepoints: mode.savepoints.clone(),
            has_untracked_savepoints: mode.has_untracked_savepoints,
            max_ident_bytes: mode.max_identifier_length.max(1),
            server_encoding: mode.server_encoding,
            saw_uncertain: false,
        }
    }

    pub(crate) fn from_mode_with_known_active(
        mode: &PostgresSessionMode,
        active_known: bool,
    ) -> Self {
        let mut tracker = Self::from_mode(mode);
        if !active_known {
            tracker.active = None;
        }
        tracker
    }

    pub(crate) fn to_session_mode(&self) -> PostgresSessionMode {
        PostgresSessionMode {
            standard_conforming_strings: self.active.unwrap_or(true),
            reset_standard_conforming_strings: self.reset_val,
            tx_state: self.tx_state,
            session_standard_conforming_strings: self.session,
            pre_tx_standard_conforming_strings: self.pre_tx,
            savepoints: self.savepoints.clone(),
            has_untracked_savepoints: self.has_untracked_savepoints,
            max_identifier_length: self.max_ident_bytes,
            server_encoding: self.server_encoding,
        }
    }

    pub(crate) fn apply(&mut self, stmt_sql: &str) {
        let _ = self.apply_with_fallback_scs(stmt_sql, true);
    }

    fn apply_with_fallback_scs(&mut self, stmt_sql: &str, fallback_scs: bool) -> bool {
        let Some(change) = parse_inline_pg_scs_change(
            stmt_sql,
            self.active.unwrap_or(fallback_scs),
            self.tx_state,
            self.max_ident_bytes,
            self.server_encoding,
        ) else {
            return false;
        };
        let mut may_change_active = false;
        if self.tx_state == Some(PostgresTxState::Aborted) {
            match change {
                PgScsChange::CommitTx { chain } | PgScsChange::RollbackTx { chain } => {
                    self.savepoints.clear();
                    self.has_untracked_savepoints = false;
                    self.session = self.pre_tx;
                    self.active = self.pre_tx;
                    self.tx_state = Some(if chain {
                        PostgresTxState::InTransaction
                    } else {
                        PostgresTxState::Idle
                    });
                    self.pre_tx = self.session;
                    may_change_active = true;
                }
                PgScsChange::PrepareTx => {
                    self.savepoints.clear();
                    self.has_untracked_savepoints = false;
                    self.session = self.pre_tx;
                    self.active = self.pre_tx;
                    self.tx_state = Some(PostgresTxState::Idle);
                    self.pre_tx = self.session;
                    may_change_active = true;
                }
                PgScsChange::RollbackToSavepoint(maybe_name) => {
                    if let Some(ref name) = maybe_name
                        && let Some(pos) = self.savepoints.iter().rposition(|sp| sp.name == *name)
                    {
                        let saved = &self.savepoints[pos];
                        self.active = saved.active_standard_conforming_strings;
                        self.session = saved.session_standard_conforming_strings;
                        self.savepoints.truncate(pos + 1);
                        self.tx_state = Some(PostgresTxState::InTransaction);
                        may_change_active = true;
                    } else if maybe_name.is_none() || self.has_untracked_savepoints {
                        self.savepoints.clear();
                        self.has_untracked_savepoints = true;
                        self.active = None;
                        self.session = None;
                        self.tx_state = None;
                        may_change_active = true;
                    }
                }
                _ => {}
            }
            if self.active.is_none() {
                self.saw_uncertain = true;
            }
            return may_change_active && self.active.is_none();
        }
        match change {
            PgScsChange::BeginTx => match self.tx_state {
                Some(PostgresTxState::InTransaction | PostgresTxState::Aborted) => {}
                Some(PostgresTxState::Idle) => {
                    self.tx_state = Some(PostgresTxState::InTransaction);
                    self.pre_tx = self.session;
                    self.savepoints.clear();
                    self.has_untracked_savepoints = false;
                }
                None => {
                    self.tx_state = Some(PostgresTxState::InTransaction);
                    self.pre_tx = None;
                    self.savepoints.clear();
                    self.has_untracked_savepoints = true;
                }
            },
            PgScsChange::CommitTx { chain } => match self.tx_state {
                Some(PostgresTxState::Idle) => {}
                Some(PostgresTxState::InTransaction) => {
                    self.savepoints.clear();
                    self.has_untracked_savepoints = false;
                    self.active = self.session;
                    self.tx_state = Some(if chain {
                        PostgresTxState::InTransaction
                    } else {
                        PostgresTxState::Idle
                    });
                    self.pre_tx = self.session;
                    may_change_active = true;
                }
                Some(PostgresTxState::Aborted) => {}
                None => {
                    self.savepoints.clear();
                    self.has_untracked_savepoints = false;
                    if self.session != self.pre_tx {
                        self.session = None;
                        self.active = None;
                    } else {
                        self.active = self.session;
                    }
                    self.tx_state = if chain {
                        None
                    } else {
                        Some(PostgresTxState::Idle)
                    };
                    self.pre_tx = self.session;
                    may_change_active = true;
                }
            },
            PgScsChange::PrepareTx => match self.tx_state {
                Some(PostgresTxState::Idle) => {}
                Some(PostgresTxState::InTransaction) => {
                    self.savepoints.clear();
                    self.has_untracked_savepoints = false;
                    self.active = self.session;
                    self.tx_state = Some(PostgresTxState::Idle);
                    self.pre_tx = self.session;
                    may_change_active = true;
                }
                Some(PostgresTxState::Aborted) => {}
                None => {
                    self.savepoints.clear();
                    self.has_untracked_savepoints = false;
                    if self.session != self.pre_tx {
                        self.session = None;
                        self.active = None;
                    } else {
                        self.active = self.session;
                    }
                    self.tx_state = Some(PostgresTxState::Idle);
                    self.pre_tx = self.session;
                    may_change_active = true;
                }
            },
            PgScsChange::RollbackTx { chain } => {
                if self.tx_state != Some(PostgresTxState::Idle) {
                    self.savepoints.clear();
                    self.has_untracked_savepoints = false;
                    let prev_none = self.tx_state.is_none();
                    self.session = self.pre_tx;
                    self.active = self.pre_tx;
                    self.tx_state = if prev_none && chain {
                        None
                    } else if chain {
                        Some(PostgresTxState::InTransaction)
                    } else {
                        Some(PostgresTxState::Idle)
                    };
                    self.pre_tx = self.session;
                    may_change_active = true;
                }
            }
            PgScsChange::Savepoint(maybe_name) => {
                if self.tx_state != Some(PostgresTxState::Idle) {
                    if let Some(name) = maybe_name {
                        if self.savepoints.len() >= MAX_TRACKED_PG_SAVEPOINTS {
                            self.savepoints.clear();
                            self.has_untracked_savepoints = true;
                        }
                        self.savepoints.push(PostgresSavepointMode {
                            name,
                            active_standard_conforming_strings: self.active,
                            session_standard_conforming_strings: self.session,
                        });
                    } else {
                        self.savepoints.clear();
                        self.has_untracked_savepoints = true;
                    }
                }
            }
            PgScsChange::ReleaseSavepoint(maybe_name) => {
                if self.tx_state != Some(PostgresTxState::Idle) {
                    if let Some(ref name) = maybe_name
                        && let Some(pos) = self.savepoints.iter().rposition(|sp| sp.name == *name)
                    {
                        self.savepoints.truncate(pos);
                    } else if maybe_name.is_none() || self.has_untracked_savepoints {
                        self.savepoints.clear();
                        self.has_untracked_savepoints = true;
                        self.tx_state = None;
                    } else if self.tx_state == Some(PostgresTxState::InTransaction) {
                        self.tx_state = Some(PostgresTxState::Aborted);
                    }
                }
            }
            PgScsChange::RollbackToSavepoint(maybe_name) => {
                if self.tx_state != Some(PostgresTxState::Idle) {
                    if let Some(ref name) = maybe_name
                        && let Some(pos) = self.savepoints.iter().rposition(|sp| sp.name == *name)
                    {
                        let saved = &self.savepoints[pos];
                        self.active = saved.active_standard_conforming_strings;
                        self.session = saved.session_standard_conforming_strings;
                        self.savepoints.truncate(pos + 1);
                        may_change_active = true;
                    } else if maybe_name.is_none() || self.has_untracked_savepoints {
                        self.savepoints.clear();
                        self.has_untracked_savepoints = true;
                        self.active = None;
                        self.session = None;
                        self.tx_state = None;
                        may_change_active = true;
                    } else if self.tx_state == Some(PostgresTxState::InTransaction) {
                        self.tx_state = Some(PostgresTxState::Aborted);
                    }
                }
            }
            PgScsChange::SetSession(val) => {
                self.session = Some(val);
                self.active = Some(val);
                if self.tx_state == Some(PostgresTxState::Idle) {
                    self.pre_tx = Some(val);
                }
                may_change_active = true;
            }
            PgScsChange::SetLocal(val) => match self.tx_state {
                Some(PostgresTxState::InTransaction) => {
                    self.active = Some(val);
                    may_change_active = true;
                }
                Some(PostgresTxState::Idle | PostgresTxState::Aborted) => {}
                None => {
                    self.active = None;
                    may_change_active = true;
                }
            },
            PgScsChange::ResetSession => {
                self.session = self.reset_val;
                self.active = self.reset_val;
                if self.tx_state == Some(PostgresTxState::Idle) {
                    self.pre_tx = self.reset_val;
                }
                may_change_active = true;
            }
            PgScsChange::ResetLocal => match self.tx_state {
                Some(PostgresTxState::InTransaction) => {
                    self.active = self.reset_val;
                    may_change_active = true;
                }
                Some(PostgresTxState::Idle | PostgresTxState::Aborted) => {}
                None => {
                    self.active = None;
                    may_change_active = true;
                }
            },
            PgScsChange::DiscardAll => match self.tx_state {
                Some(PostgresTxState::Idle) => {
                    self.session = self.reset_val;
                    self.active = self.reset_val;
                    self.pre_tx = self.reset_val;
                    may_change_active = true;
                }
                Some(PostgresTxState::InTransaction | PostgresTxState::Aborted) => {}
                None => {
                    self.session = None;
                    self.active = None;
                    may_change_active = true;
                }
            },
            PgScsChange::ReloadConf => {
                self.reset_val = None;
            }
            PgScsChange::UncertainSetting
                if self.active.is_some()
                    || !postgres_slice_has_multiple_statements(stmt_sql, !fallback_scs) =>
            {
                self.active = None;
                self.session = None;
                if self.tx_state != Some(PostgresTxState::InTransaction) {
                    self.pre_tx = None;
                }
                may_change_active = true;
            }
            PgScsChange::Uncertain | PgScsChange::UncertainSetting => {
                self.active = None;
                self.session = None;
                self.tx_state = None;
                self.pre_tx = None;
                self.savepoints.clear();
                self.has_untracked_savepoints = true;
                may_change_active = true;
            }
        }
        if self.active.is_none() {
            self.saw_uncertain = true;
        }
        may_change_active && self.active.is_none()
    }
}

pub(crate) fn parse_pg_bool_guc(raw: &str) -> Option<bool> {
    if raw == "1" {
        return Some(true);
    }
    if raw == "0" {
        return Some(false);
    }
    match raw.to_ascii_lowercase().as_str() {
        "t" | "tr" | "tru" | "true" | "y" | "ye" | "yes" | "on" => Some(true),
        "f" | "fa" | "fal" | "fals" | "false" | "n" | "no" | "of" | "off" => Some(false),
        _ => None,
    }
}

fn eval_pg_set_config_bool_arg(
    index: &CredTokIndex<'_>,
    range: std::ops::Range<usize>,
) -> Option<bool> {
    let stripped = index.strip_wrapping_parens(range.clone());
    match &index.toks[stripped.clone()] {
        [CredTok::Word(w) | CredTok::DoubleQuoted(w) | CredTok::Number(w)] => parse_pg_bool_guc(w),
        _ => pg_eval_simple_string_literal(index, range).and_then(|s| parse_pg_bool_guc(&s)),
    }
}

struct SetConfigCallArgs {
    setting: std::ops::Range<usize>,
    value: std::ops::Range<usize>,
    is_local: Option<std::ops::Range<usize>>,
}

fn visit_set_config_calls<T>(
    toks: &[CredTok],
    mut f: impl FnMut(SetConfigCallArgs) -> std::ops::ControlFlow<T>,
) -> Option<T> {
    enum StackEntry {
        SetConfig {
            arg_start: usize,
            args: Vec<std::ops::Range<usize>>,
        },
        Paren,
        Bracket,
    }

    let mut call_stack: Vec<StackEntry> = Vec::new();
    let mut open_parens = 0usize;
    let mut open_brackets = 0usize;

    for (idx, tok) in toks.iter().enumerate() {
        match tok {
            CredTok::Punct('(') => {
                let is_set_config = idx > 0
                    && matches!(
                        &toks[idx - 1],
                        CredTok::Word(name) | CredTok::DoubleQuoted(name)
                            if name.eq_ignore_ascii_case("SET_CONFIG")
                    );
                open_parens += 1;
                call_stack.push(if is_set_config {
                    StackEntry::SetConfig {
                        arg_start: idx + 1,
                        args: Vec::new(),
                    }
                } else {
                    StackEntry::Paren
                });
            }
            CredTok::Punct('[') => {
                open_brackets += 1;
                call_stack.push(StackEntry::Bracket);
            }
            CredTok::Punct(',') => {
                if let Some(StackEntry::SetConfig { arg_start, args }) = call_stack.last_mut() {
                    args.push(*arg_start..idx);
                    *arg_start = idx + 1;
                }
            }
            CredTok::Punct(']') if open_brackets > 0 => {
                while let Some(entry) = call_stack.pop() {
                    match entry {
                        StackEntry::Bracket => {
                            open_brackets -= 1;
                            break;
                        }
                        StackEntry::Paren | StackEntry::SetConfig { .. } => {
                            open_parens -= 1;
                        }
                    }
                }
            }
            CredTok::Punct(')') if open_parens > 0 => {
                let mut closed = None;
                while let Some(entry) = call_stack.pop() {
                    match entry {
                        StackEntry::Bracket => {
                            open_brackets -= 1;
                        }
                        StackEntry::Paren => {
                            open_parens -= 1;
                            break;
                        }
                        StackEntry::SetConfig { arg_start, args } => {
                            open_parens -= 1;
                            closed = Some((arg_start, args));
                            break;
                        }
                    }
                }
                let Some((arg_start, mut args)) = closed else {
                    continue;
                };
                args.push(arg_start..idx);
                if args.len() < 2 {
                    continue;
                }
                let mut setting_arg: Option<std::ops::Range<usize>> = None;
                let mut value_arg: Option<std::ops::Range<usize>> = None;
                let mut local_arg: Option<std::ops::Range<usize>> = None;
                let mut positional_idx = 0usize;
                let mut seen_named = false;
                for arg_range in args {
                    let named = match &toks[arg_range.clone()] {
                        [
                            CredTok::Word(p) | CredTok::DoubleQuoted(p),
                            CredTok::Punct('='),
                            CredTok::Punct('>'),
                            ..,
                        ]
                        | [
                            CredTok::Word(p) | CredTok::DoubleQuoted(p),
                            CredTok::Punct(':'),
                            CredTok::Punct('='),
                            ..,
                        ] => Some((p.as_str(), (arg_range.start + 3)..arg_range.end)),
                        _ => None,
                    };
                    if let Some((param, val_range)) = named {
                        seen_named = true;
                        if param.eq_ignore_ascii_case("setting_name") {
                            setting_arg = Some(val_range);
                        } else if param.eq_ignore_ascii_case("new_value") {
                            value_arg = Some(val_range);
                        } else if param.eq_ignore_ascii_case("is_local") {
                            local_arg = Some(val_range);
                        }
                    } else if !seen_named {
                        match positional_idx {
                            0 => setting_arg = Some(arg_range),
                            1 => value_arg = Some(arg_range),
                            2 => local_arg = Some(arg_range),
                            _ => {}
                        }
                        positional_idx += 1;
                    }
                }
                let (Some(setting), Some(value)) = (setting_arg, value_arg) else {
                    continue;
                };
                if let std::ops::ControlFlow::Break(res) = f(SetConfigCallArgs {
                    setting,
                    value,
                    is_local: local_arg,
                }) {
                    return Some(res);
                }
            }
            _ => {}
        }
    }
    None
}

fn pg_savepoint_ident(
    tok: &CredTok,
    max_ident_bytes: usize,
    server_encoding: Option<PostgresServerEncoding>,
) -> Option<Option<String>> {
    let limit = max_ident_bytes.max(1);
    match tok {
        CredTok::Word(w) => {
            if w.len() >= limit && w.as_bytes()[..limit].is_ascii() {
                Some(Some(w[..limit].to_ascii_lowercase()))
            } else if w.is_ascii() || server_encoding == Some(PostgresServerEncoding::Utf8) {
                let lower = w.to_ascii_lowercase();
                let end = clamp_char_boundary(&lower, limit);
                Some(Some(lower[..end].to_owned()))
            } else {
                // Single-byte and Other encodings can apply locale-dependent high-bit folding or unknown byte widths.
                Some(None)
            }
        }
        CredTok::DoubleQuoted(s) if !s.is_empty() => {
            Some(truncate_pg_quoted_savepoint_name(s, limit, server_encoding))
        }
        _ => None,
    }
}

fn pg_parse_tx_end_chain(rest: &[CredTok]) -> Option<bool> {
    let trimmed = match rest.last() {
        Some(CredTok::Punct(';')) => &rest[..rest.len() - 1],
        _ => rest,
    };
    let after_scope = match trimmed {
        [CredTok::Word(scope), tail @ ..] if scope == "WORK" || scope == "TRANSACTION" => tail,
        _ => trimmed,
    };
    match after_scope {
        [] => Some(false),
        [CredTok::Word(and_kw), CredTok::Word(chain_kw)]
            if and_kw == "AND" && chain_kw == "CHAIN" =>
        {
            Some(true)
        }
        [CredTok::Word(and_kw), CredTok::Word(no_kw), CredTok::Word(chain_kw)]
            if and_kw == "AND" && no_kw == "NO" && chain_kw == "CHAIN" =>
        {
            Some(false)
        }
        _ => None,
    }
}

fn pg_is_unconditional_single_set_config_select(toks: &[CredTok], index: &CredTokIndex<'_>) -> bool {
    let trimmed = if toks
        .last()
        .is_some_and(|t| matches!(t, CredTok::Punct(';')))
    {
        &toks[..toks.len() - 1]
    } else {
        toks
    };
    if !matches!(trimmed.first(), Some(CredTok::Word(s)) if s == "SELECT") {
        return false;
    }
    let fn_pos = match trimmed.get(1..) {
        Some(
            [
                CredTok::Word(schema) | CredTok::DoubleQuoted(schema),
                CredTok::Punct('.'),
                CredTok::Word(f) | CredTok::DoubleQuoted(f),
                CredTok::Punct('('),
                ..,
            ],
        ) if schema.eq_ignore_ascii_case("PG_CATALOG") && f.eq_ignore_ascii_case("SET_CONFIG") => 3,
        Some([CredTok::Word(f) | CredTok::DoubleQuoted(f), CredTok::Punct('('), ..])
            if f.eq_ignore_ascii_case("SET_CONFIG") =>
        {
            1
        }
        _ => return false,
    };
    let open_idx = fn_pos + 1;
    let close_idx = index.matching_close[open_idx];
    if close_idx <= open_idx || close_idx >= trimmed.len() {
        return false;
    }
    match &trimmed[(close_idx + 1)..] {
        []
        | [CredTok::Word(_) | CredTok::DoubleQuoted(_)] => true,
        [CredTok::Word(as_kw), CredTok::Word(_) | CredTok::DoubleQuoted(_)] if as_kw == "AS" => {
            true
        }
        _ => false,
    }
}

fn parse_inline_pg_scs_set_config(toks: &[CredTok]) -> Option<PgScsChange> {
    let index = CredTokIndex::new(toks);
    let is_unconditional = pg_is_unconditional_single_set_config_select(toks, &index);
    let mut call_count = 0usize;
    let mut definite_change: Option<PgScsChange> = None;
    let mut may_touch_scs = false;

    let _: Option<()> = visit_set_config_calls(toks, |call| {
        call_count += 1;
        let Some(setting_name) = pg_eval_simple_string_literal(&index, call.setting) else {
            may_touch_scs = true;
            return std::ops::ControlFlow::Break(());
        };
        if !setting_name.eq_ignore_ascii_case("standard_conforming_strings") {
            return std::ops::ControlFlow::Continue(());
        }
        may_touch_scs = true;
        if !is_unconditional || call_count > 1 {
            definite_change = None;
            return std::ops::ControlFlow::Break(());
        }
        let Some(local_range) = call.is_local else {
            definite_change = None;
            return std::ops::ControlFlow::Break(());
        };
        let Some(is_local) = eval_pg_set_config_bool_arg(&index, local_range) else {
            definite_change = None;
            return std::ops::ControlFlow::Break(());
        };
        if pg_expr_is_null(&index, call.value.clone(), 0) {
            definite_change = Some(if is_local {
                PgScsChange::ResetLocal
            } else {
                PgScsChange::ResetSession
            });
        } else if let Some(val) = eval_pg_set_config_bool_arg(&index, call.value) {
            definite_change = Some(if is_local {
                PgScsChange::SetLocal(val)
            } else {
                PgScsChange::SetSession(val)
            });
        } else {
            definite_change = None;
            return std::ops::ControlFlow::Break(());
        }
        std::ops::ControlFlow::Continue(())
    });

    if let Some(change) = definite_change
        && call_count == 1
        && is_unconditional
    {
        return Some(change);
    }
    if may_touch_scs {
        if is_pg_select_like_toks(toks) {
            Some(PgScsChange::UncertainSetting)
        } else {
            Some(PgScsChange::Uncertain)
        }
    } else {
        None
    }
}

pub(crate) fn parse_inline_pg_scs_change(
    stmt_sql: &str,
    active_scs: bool,
    tx_state: Option<PostgresTxState>,
    max_ident_bytes: usize,
    server_encoding: Option<PostgresServerEncoding>,
) -> Option<PgScsChange> {
    let has_scs_kw = contains_ascii_ci(stmt_sql, "standard_conforming_strings");
    let has_set_config = contains_ascii_ci(stmt_sql, "set_config");
    let has_reset = contains_ascii_ci(stmt_sql, "reset");
    let has_discard = contains_ascii_ci(stmt_sql, "discard");
    let has_reload = contains_ascii_ci(stmt_sql, "pg_reload_conf");
    let has_proc_or_exec = contains_ascii_ci(stmt_sql, "call")
        || contains_ascii_ci(stmt_sql, "execute")
        || contains_ascii_ci(stmt_sql, "do");
    let has_tx_kw = contains_ascii_ci(stmt_sql, "begin")
        || contains_ascii_ci(stmt_sql, "start")
        || contains_ascii_ci(stmt_sql, "commit")
        || contains_ascii_ci(stmt_sql, "end")
        || contains_ascii_ci(stmt_sql, "rollback")
        || contains_ascii_ci(stmt_sql, "abort")
        || contains_ascii_ci(stmt_sql, "savepoint")
        || contains_ascii_ci(stmt_sql, "release")
        || contains_ascii_ci(stmt_sql, "prepare");
    let has_any_tx = !matches!(tx_state, Some(PostgresTxState::Idle)) || has_tx_kw;
    if !has_scs_kw
        && !has_set_config
        && !has_reset
        && !has_discard
        && !has_reload
        && !has_proc_or_exec
        && !has_tx_kw
    {
        return None;
    }

    let toks = tokenize_statement_for_credentials_with_mode(
        DbEngine::Postgres,
        stmt_sql,
        MysqlQuoteMode::DEFAULT,
        MysqlCommentPolicy::UnconditionalOnly,
        active_scs,
    );
    match toks.as_slice() {
        [CredTok::Word(a), rest @ ..] if a == "SET" => {
            let (is_local, after_scope) = match rest {
                [CredTok::Word(scope), tail @ ..] if scope == "LOCAL" => (true, tail),
                [CredTok::Word(scope), tail @ ..] if scope == "SESSION" => (false, tail),
                _ => (false, rest),
            };
            if let [
                CredTok::Word(guc) | CredTok::DoubleQuoted(guc),
                op_and_val @ ..,
            ] = after_scope
                && guc.eq_ignore_ascii_case("standard_conforming_strings")
            {
                let rhs = match op_and_val {
                    [CredTok::Punct('='), tail @ ..] => Some(tail),
                    [CredTok::Word(op), tail @ ..] if op == "TO" => Some(tail),
                    _ => None,
                };
                if let Some(val_toks) = rhs {
                    match val_toks {
                        [CredTok::Word(v)] if v == "DEFAULT" => {
                            return Some(if is_local {
                                PgScsChange::ResetLocal
                            } else {
                                PgScsChange::ResetSession
                            });
                        }
                        [
                            CredTok::Word(v)
                            | CredTok::Literal(v)
                            | CredTok::DoubleQuoted(v)
                            | CredTok::Number(v),
                        ] => {
                            if let Some(val) = parse_pg_bool_guc(v) {
                                return Some(if is_local {
                                    PgScsChange::SetLocal(val)
                                } else {
                                    PgScsChange::SetSession(val)
                                });
                            }
                            return Some(PgScsChange::Uncertain);
                        }
                        _ => return Some(PgScsChange::Uncertain),
                    }
                }
                return Some(PgScsChange::Uncertain);
            }
        }
        [CredTok::Word(a), CredTok::Word(b)]
            if a == "RESET"
                && (b == "ALL" || b.eq_ignore_ascii_case("standard_conforming_strings")) =>
        {
            return Some(PgScsChange::ResetSession);
        }
        [CredTok::Word(a), CredTok::DoubleQuoted(b)]
            if a == "RESET" && b.eq_ignore_ascii_case("standard_conforming_strings") =>
        {
            return Some(PgScsChange::ResetSession);
        }
        [CredTok::Word(a), CredTok::Word(b)] if a == "DISCARD" && b == "ALL" => {
            return Some(PgScsChange::DiscardAll);
        }
        [CredTok::Word(a), rest @ ..]
            if a == "BEGIN" && !matches!(rest.first(), Some(CredTok::Word(b)) if b == "ATOMIC") =>
        {
            return Some(PgScsChange::BeginTx);
        }
        [CredTok::Word(a), CredTok::Word(b), ..] if a == "START" && b == "TRANSACTION" => {
            return Some(PgScsChange::BeginTx);
        }
        [CredTok::Word(a), rest @ ..]
            if (a == "COMMIT" || a == "END")
                && !matches!(rest.first(), Some(CredTok::Word(w)) if w == "PREPARED") =>
        {
            if let Some(chain) = pg_parse_tx_end_chain(rest) {
                return Some(PgScsChange::CommitTx { chain });
            }
        }
        [CredTok::Word(a), CredTok::Word(b), ..] if a == "PREPARE" && b == "TRANSACTION" => {
            return Some(PgScsChange::PrepareTx);
        }
        [CredTok::Word(a), name_tok, ..] if a == "SAVEPOINT" => {
            if has_any_tx
                && let Some(name) = pg_savepoint_ident(name_tok, max_ident_bytes, server_encoding)
            {
                return Some(PgScsChange::Savepoint(name));
            }
        }
        [CredTok::Word(a), rest @ ..] if a == "RELEASE" => {
            let name_tok = match rest {
                [CredTok::Word(sp), tok, ..] if sp == "SAVEPOINT" => Some(tok),
                [tok, ..] => Some(tok),
                _ => None,
            };
            if has_any_tx
                && let Some(tok) = name_tok
                && let Some(name) = pg_savepoint_ident(tok, max_ident_bytes, server_encoding)
            {
                return Some(PgScsChange::ReleaseSavepoint(name));
            }
        }
        [CredTok::Word(a), rest @ ..]
            if (a == "ROLLBACK" || a == "ABORT")
                && !matches!(rest.first(), Some(CredTok::Word(w)) if w == "PREPARED") =>
        {
            let sp_tail = if a == "ROLLBACK" {
                match rest {
                    [CredTok::Word(w), tail @ ..] if w == "TO" => Some(tail),
                    [CredTok::Word(scope), CredTok::Word(to), tail @ ..]
                        if (scope == "WORK" || scope == "TRANSACTION") && to == "TO" =>
                    {
                        Some(tail)
                    }
                    _ => None,
                }
            } else {
                None
            };
            if let Some(tail) = sp_tail {
                let name_tok = match tail {
                    [CredTok::Word(sp), tok, ..] if sp == "SAVEPOINT" => Some(tok),
                    [tok, ..] => Some(tok),
                    _ => None,
                };
                if has_any_tx
                    && let Some(tok) = name_tok
                    && let Some(name) = pg_savepoint_ident(tok, max_ident_bytes, server_encoding)
                {
                    return Some(PgScsChange::RollbackToSavepoint(name));
                }
            } else if let Some(chain) = pg_parse_tx_end_chain(rest) {
                return Some(PgScsChange::RollbackTx { chain });
            }
        }
        [CredTok::Word(a), ..] if a == "CALL" || a == "EXECUTE" => {
            return Some(PgScsChange::Uncertain);
        }
        [CredTok::Word(a), rest @ ..]
            if a == "EXPLAIN" && rest.iter().any(|t| matches!(t, CredTok::Word(w) if w == "EXECUTE")) =>
        {
            return Some(PgScsChange::Uncertain);
        }
        [CredTok::Word(a), ..]
            if a == "DO"
                && (has_scs_kw
                    || has_set_config
                    || has_reset
                    || has_discard
                    || contains_ascii_ci(stmt_sql, "set")) =>
        {
            return Some(PgScsChange::Uncertain);
        }
        _ => {}
    }

    if has_reload {
        return Some(PgScsChange::ReloadConf);
    }
    if has_set_config {
        return parse_inline_pg_scs_set_config(&toks);
    }
    None
}

/// Rewrites PostgreSQL ordinary `'...'` / `N'...'` string literals containing backslashes into
/// `E'...'` syntax when `standard_conforming_strings = off` so SQL AST parsers handle `\'` escapes.
pub(crate) fn pg_normalize_legacy_escape_strings(sql: &str) -> std::borrow::Cow<'_, str> {
    if !sql.contains('\\') || !sql.contains('\'') {
        return std::borrow::Cow::Borrowed(sql);
    }
    let bytes = sql.as_bytes();
    let len = bytes.len();
    let mut out = String::new();
    let mut copied_up_to = 0usize;
    let mut i = 0usize;
    let mut state = ScanState::Normal;
    let mut dollar_tag: Option<String> = None;

    let push_prefix_e = |out: &mut String, sql: &str, copied_up_to: &mut usize, lit_start: usize, skip_prefix: usize| {
        if out.is_empty() {
            out.reserve(sql.len() + 16);
        }
        out.push_str(&sql[*copied_up_to..lit_start]);
        let need_space = out
            .chars()
            .next_back()
            .is_some_and(|c| is_ident_continue_for_engine(DbEngine::Postgres, c));
        if need_space {
            out.push(' ');
        }
        out.push('E');
        *copied_up_to = lit_start + skip_prefix;
    };

    while i < len {
        if let Some(ref tag) = dollar_tag {
            if bytes[i] == b'$' && sql[i..].starts_with(tag.as_str()) {
                i += tag.len();
                dollar_tag = None;
            } else {
                i += char_len_at(sql, i);
            }
            continue;
        }

        match state {
            ScanState::Normal => {
                if bytes[i] == b'\'' {
                    let lit_start = i;
                    i += 1;
                    let mut saw_bs = false;
                    while i < len {
                        if bytes[i] == b'\\' {
                            saw_bs = true;
                            i += 1;
                            if i < len {
                                i += char_len_at(sql, i);
                            }
                        } else if bytes[i] == b'\'' {
                            i += 1;
                            if i < len && bytes[i] == b'\'' {
                                i += 1;
                            } else {
                                break;
                            }
                        } else {
                            i += char_len_at(sql, i);
                        }
                    }
                    if saw_bs {
                        push_prefix_e(&mut out, sql, &mut copied_up_to, lit_start, 0);
                    }
                } else if (bytes[i] == b'N' || bytes[i] == b'n')
                    && i + 1 < len
                    && bytes[i + 1] == b'\''
                    && !preceded_by_ident(DbEngine::Postgres, sql, i)
                {
                    let lit_start = i;
                    i += 2;
                    let mut saw_bs = false;
                    while i < len {
                        if bytes[i] == b'\\' {
                            saw_bs = true;
                            i += 1;
                            if i < len {
                                i += char_len_at(sql, i);
                            }
                        } else if bytes[i] == b'\'' {
                            i += 1;
                            if i < len && bytes[i] == b'\'' {
                                i += 1;
                            } else {
                                break;
                            }
                        } else {
                            i += char_len_at(sql, i);
                        }
                    }
                    if saw_bs {
                        push_prefix_e(&mut out, sql, &mut copied_up_to, lit_start, 1);
                    }
                } else if (bytes[i] == b'E' || bytes[i] == b'e')
                    && i + 1 < len
                    && bytes[i + 1] == b'\''
                    && !preceded_by_ident(DbEngine::Postgres, sql, i)
                {
                    i += 2;
                    state = ScanState::EscapeSingleQuote;
                } else if (bytes[i] == b'U' || bytes[i] == b'u')
                    && i + 2 < len
                    && bytes[i + 1] == b'&'
                    && bytes[i + 2] == b'\''
                    && !preceded_by_ident(DbEngine::Postgres, sql, i)
                {
                    i += 3;
                    state = ScanState::PlainSingleQuote;
                } else if matches!(bytes[i], b'X' | b'x' | b'B' | b'b')
                    && i + 1 < len
                    && bytes[i + 1] == b'\''
                    && !preceded_by_ident(DbEngine::Postgres, sql, i)
                {
                    i += 2;
                    state = ScanState::PlainSingleQuote;
                } else if bytes[i] == b'"' {
                    i += 1;
                    state = ScanState::DoubleQuote;
                } else if is_dash_comment_start(DbEngine::Postgres, sql, i) {
                    i += 2;
                    state = ScanState::LineComment;
                } else if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
                    i += 2;
                    state = ScanState::BlockComment(1);
                } else if bytes[i] == b'$'
                    && !preceded_by_ident(DbEngine::Postgres, sql, i)
                    && let Some(tag) = parse_dollar_tag(&sql[i..])
                {
                    i += tag.len();
                    dollar_tag = Some(tag.to_owned());
                } else {
                    i += char_len_at(sql, i);
                }
            }
            ScanState::EscapeSingleQuote => {
                if bytes[i] == b'\\' {
                    i += 1;
                    if i < len {
                        i += char_len_at(sql, i);
                    }
                } else if bytes[i] == b'\'' {
                    i += 1;
                    if i < len && bytes[i] == b'\'' {
                        i += 1;
                    } else {
                        state = ScanState::Normal;
                    }
                } else {
                    i += char_len_at(sql, i);
                }
            }
            ScanState::PlainSingleQuote | ScanState::SingleQuote => {
                if bytes[i] == b'\'' {
                    i += 1;
                    if i < len && bytes[i] == b'\'' {
                        i += 1;
                    } else {
                        state = ScanState::Normal;
                    }
                } else {
                    i += char_len_at(sql, i);
                }
            }
            ScanState::DoubleQuote => {
                if bytes[i] == b'"' {
                    i += 1;
                    if i < len && bytes[i] == b'"' {
                        i += 1;
                    } else {
                        state = ScanState::Normal;
                    }
                } else {
                    i += char_len_at(sql, i);
                }
            }
            ScanState::LineComment => {
                if bytes[i] == b'\n' || bytes[i] == b'\r' {
                    state = ScanState::Normal;
                }
                i += char_len_at(sql, i);
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
                    i += char_len_at(sql, i);
                }
            }
            ScanState::BacktickQuote | ScanState::BracketQuote => {
                i += char_len_at(sql, i);
            }
        }
    }

    if out.is_empty() {
        std::borrow::Cow::Borrowed(sql)
    } else {
        out.push_str(&sql[copied_up_to..]);
        std::borrow::Cow::Owned(out)
    }
}

fn parse_pg_byte_escape(bytes: &[u8], i: usize) -> Option<(u8, usize)> {
    if bytes.get(i) != Some(&b'\\') {
        return None;
    }
    let next = *bytes.get(i + 1)?;
    if next == b'x' {
        let mut val = 0u8;
        let mut end = i + 2;
        while end < bytes.len() && end < i + 4 {
            let digit = match bytes[end] {
                b'0'..=b'9' => bytes[end] - b'0',
                b'a'..=b'f' => bytes[end] - b'a' + 10,
                b'A'..=b'F' => bytes[end] - b'A' + 10,
                _ => break,
            };
            val = val * 16 + digit;
            end += 1;
        }
        (end > i + 2).then_some((val, end))
    } else if (b'0'..=b'7').contains(&next) {
        let mut val = 0u8;
        let mut end = i + 1;
        while end < bytes.len() && end < i + 4 && (b'0'..=b'7').contains(&bytes[end]) {
            val = val.wrapping_mul(8).wrapping_add(bytes[end] - b'0');
            end += 1;
        }
        Some((val, end))
    } else {
        None
    }
}

fn normalize_pg_escape_literal_bytes(body: &str) -> std::borrow::Cow<'_, str> {
    if !body.contains('\\') {
        return std::borrow::Cow::Borrowed(body);
    }
    let bytes = body.as_bytes();
    let len = bytes.len();
    let mut out = String::new();
    let mut copied = 0usize;
    let mut i = 0usize;

    while i < len {
        if bytes[i] == b'\\' {
            if let Some((first_byte, mut run_end)) = parse_pg_byte_escape(bytes, i) {
                let run_start = i;
                let mut run_bytes = vec![first_byte];
                while let Some((next_byte, next_end)) = parse_pg_byte_escape(bytes, run_end) {
                    run_bytes.push(next_byte);
                    run_end = next_end;
                }
                if run_bytes.iter().any(|&b| b >= 0x80)
                    && !run_bytes.contains(&0)
                    && let Ok(decoded) = std::str::from_utf8(&run_bytes)
                {
                    if out.is_empty() {
                        out.reserve(body.len());
                    }
                    out.push_str(&body[copied..run_start]);
                    out.push_str(decoded);
                    copied = run_end;
                }
                i = run_end;
            } else {
                i += 1;
                if i < len {
                    i += char_len_at(body, i);
                }
            }
        } else {
            i += char_len_at(body, i);
        }
    }

    if out.is_empty() {
        std::borrow::Cow::Borrowed(body)
    } else {
        out.push_str(&body[copied..]);
        std::borrow::Cow::Owned(out)
    }
}

fn pg_normalize_byte_escapes_for_parser(sql: &str) -> std::borrow::Cow<'_, str> {
    if !sql.contains('\\') || !sql.contains('\'') {
        return std::borrow::Cow::Borrowed(sql);
    }
    let bytes = sql.as_bytes();
    let len = bytes.len();
    let mut out = String::new();
    let mut copied_up_to = 0usize;
    let mut i = 0usize;
    let mut state = ScanState::Normal;
    let mut dollar_tag: Option<String> = None;
    let mut escape_cont = EscapeContinuation::None;

    while i < len {
        if let Some(ref tag) = dollar_tag {
            if bytes[i] == b'$' && sql[i..].starts_with(tag.as_str()) {
                i += tag.len();
                dollar_tag = None;
            } else {
                i += char_len_at(sql, i);
            }
            continue;
        }

        match state {
            ScanState::Normal => {
                if ((bytes[i] == b'E' || bytes[i] == b'e')
                    && i + 1 < len
                    && bytes[i + 1] == b'\''
                    && !preceded_by_ident(DbEngine::Postgres, sql, i))
                    || (bytes[i] == b'\'' && escape_cont == EscapeContinuation::AfterNewline)
                {
                    let body_start = if bytes[i] == b'\'' { i + 1 } else { i + 2 };
                    let mut j = body_start;
                    let mut body_end = len;
                    let mut closed = false;
                    while j < len {
                        if bytes[j] == b'\\' {
                            j += 1;
                            if j < len {
                                j += char_len_at(sql, j);
                            }
                        } else if bytes[j] == b'\'' {
                            if j + 1 < len && bytes[j + 1] == b'\'' {
                                j += 2;
                            } else {
                                body_end = j;
                                j += 1;
                                closed = true;
                                break;
                            }
                        } else {
                            j += char_len_at(sql, j);
                        }
                    }
                    if let std::borrow::Cow::Owned(norm_body) =
                        normalize_pg_escape_literal_bytes(&sql[body_start..body_end])
                    {
                        if out.is_empty() {
                            out.reserve(sql.len());
                        }
                        out.push_str(&sql[copied_up_to..body_start]);
                        out.push_str(&norm_body);
                        copied_up_to = body_end;
                    }
                    escape_cont = if closed {
                        EscapeContinuation::AfterClose
                    } else {
                        EscapeContinuation::None
                    };
                    i = j;
                } else if bytes[i] == b'\'' {
                    escape_cont = EscapeContinuation::None;
                    i += 1;
                    state = ScanState::PlainSingleQuote;
                } else if (bytes[i] == b'U' || bytes[i] == b'u')
                    && i + 2 < len
                    && bytes[i + 1] == b'&'
                    && bytes[i + 2] == b'\''
                    && !preceded_by_ident(DbEngine::Postgres, sql, i)
                {
                    escape_cont = EscapeContinuation::None;
                    i += 3;
                    state = ScanState::PlainSingleQuote;
                } else if matches!(bytes[i], b'X' | b'x' | b'B' | b'b')
                    && i + 1 < len
                    && bytes[i + 1] == b'\''
                    && !preceded_by_ident(DbEngine::Postgres, sql, i)
                {
                    escape_cont = EscapeContinuation::None;
                    i += 2;
                    state = ScanState::PlainSingleQuote;
                } else if bytes[i] == b'"' {
                    escape_cont = EscapeContinuation::None;
                    i += 1;
                    state = ScanState::DoubleQuote;
                } else if is_dash_comment_start(DbEngine::Postgres, sql, i) {
                    i += 2;
                    state = ScanState::LineComment;
                } else if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
                    i += 2;
                    state = ScanState::BlockComment(1);
                } else if bytes[i] == b'$'
                    && !preceded_by_ident(DbEngine::Postgres, sql, i)
                    && let Some(tag) = parse_dollar_tag(&sql[i..])
                {
                    escape_cont = EscapeContinuation::None;
                    i += tag.len();
                    dollar_tag = Some(tag.to_owned());
                } else {
                    let ch = sql[i..].chars().next().unwrap_or('\0');
                    if ch == '\n' || ch == '\r' {
                        if escape_cont == EscapeContinuation::AfterClose {
                            escape_cont = EscapeContinuation::AfterNewline;
                        }
                    } else if !is_sql_whitespace(DbEngine::Postgres, ch) {
                        escape_cont = EscapeContinuation::None;
                    }
                    i += ch.len_utf8();
                }
            }
            ScanState::PlainSingleQuote => {
                if bytes[i] == b'\'' {
                    i += 1;
                    if i < len && bytes[i] == b'\'' {
                        i += 1;
                    } else {
                        state = ScanState::Normal;
                        escape_cont = EscapeContinuation::PlainAfterClose;
                    }
                } else {
                    i += char_len_at(sql, i);
                }
            }
            ScanState::DoubleQuote => {
                if bytes[i] == b'"' {
                    i += 1;
                    if i < len && bytes[i] == b'"' {
                        i += 1;
                    } else {
                        state = ScanState::Normal;
                    }
                } else {
                    i += char_len_at(sql, i);
                }
            }
            ScanState::LineComment => {
                if bytes[i] == b'\n' || bytes[i] == b'\r' {
                    if escape_cont == EscapeContinuation::AfterClose {
                        escape_cont = EscapeContinuation::AfterNewline;
                    }
                    state = ScanState::Normal;
                }
                i += char_len_at(sql, i);
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
                    i += char_len_at(sql, i);
                }
            }
            ScanState::SingleQuote
            | ScanState::EscapeSingleQuote
            | ScanState::BacktickQuote
            | ScanState::BracketQuote => {
                i += char_len_at(sql, i);
            }
        }
    }

    if out.is_empty() {
        std::borrow::Cow::Borrowed(sql)
    } else {
        out.push_str(&sql[copied_up_to..]);
        std::borrow::Cow::Owned(out)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SetModeTok {
    Word(String),
    Str(String),
    Backtick(String),
    DoubleAt,
    At,
    Colon,
    Dot,
    Comma,
    Eq,
    LParen,
    RParen,
    Other,
}

fn tokenize_mysql_set_sql_mode(sql: &str, mode: MysqlQuoteMode) -> Vec<SetModeTok> {
    let bytes = sql.as_bytes();
    let len = bytes.len();
    let mut toks = Vec::new();
    let mut i = 0usize;
    let allow_sq_bs = mode.allow_single_quote_backslash();
    let allow_dq_bs = mode.allow_double_quote_backslash();

    while i < len {
        let ch = sql[i..].chars().next().unwrap_or('\0');
        if is_sql_whitespace(DbEngine::Mysql, ch) {
            i += ch.len_utf8();
        } else if is_dash_comment_start(DbEngine::Mysql, sql, i) {
            i += 2;
            while i < len && bytes[i] != b'\n' {
                i += char_len_at(sql, i);
            }
        } else if bytes[i] == b'#' {
            i += 1;
            while i < len && bytes[i] != b'\n' {
                i += char_len_at(sql, i);
            }
        } else if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            while i < len {
                if i + 1 < len && bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    i += 2;
                    break;
                }
                i += char_len_at(sql, i);
            }
        } else if bytes[i] == b'\'' {
            let (next_i, val, closed) = read_quoted_raw(sql, i, b'\'', allow_sq_bs);
            if !closed {
                toks.push(SetModeTok::Other);
                break;
            }
            i = next_i;
            toks.push(SetModeTok::Str(val));
        } else if bytes[i] == b'"' {
            let (next_i, val, closed) = read_quoted_raw(sql, i, b'"', allow_dq_bs);
            if !closed {
                toks.push(SetModeTok::Other);
                break;
            }
            i = next_i;
            if mode.ansi_quotes {
                toks.push(SetModeTok::Backtick(val));
            } else {
                toks.push(SetModeTok::Str(val));
            }
        } else if bytes[i] == b'`' {
            let (next_i, ident, closed) = read_quoted_raw(sql, i, b'`', false);
            if !closed {
                toks.push(SetModeTok::Other);
                break;
            }
            i = next_i;
            toks.push(SetModeTok::Backtick(ident));
        } else if mode.allow_bracket_quotes() && bytes[i] == b'[' {
            let (next_i, ident, closed) = read_bracket_raw(sql, i, true);
            if !closed {
                toks.push(SetModeTok::Other);
                break;
            }
            i = next_i;
            toks.push(SetModeTok::Backtick(ident));
        } else if i + 1 < len && bytes[i] == b'@' && bytes[i + 1] == b'@' {
            i += 2;
            toks.push(SetModeTok::DoubleAt);
        } else if bytes[i] == b'@' {
            i += 1;
            toks.push(SetModeTok::At);
        } else if i + 1 < len && bytes[i] == b':' && bytes[i + 1] == b'=' {
            i += 2;
            toks.push(SetModeTok::Eq);
        } else if bytes[i] == b':' {
            i += 1;
            toks.push(SetModeTok::Colon);
        } else if matches!(ch, '.' | ',' | '=' | '(' | ')') {
            i += 1;
            toks.push(match ch {
                '.' => SetModeTok::Dot,
                ',' => SetModeTok::Comma,
                '=' => SetModeTok::Eq,
                '(' => SetModeTok::LParen,
                ')' => SetModeTok::RParen,
                _ => SetModeTok::Other,
            });
        } else if is_ident_start_for_engine(DbEngine::Mysql, ch) {
            let start = i;
            i += ch.len_utf8();
            while i < len {
                let next_ch = sql[i..].chars().next().unwrap_or('\0');
                if !is_ident_continue_for_engine(DbEngine::Mysql, next_ch) {
                    break;
                }
                i += next_ch.len_utf8();
            }
            toks.push(SetModeTok::Word(sql[start..i].to_ascii_uppercase()));
        } else {
            i += ch.len_utf8();
            toks.push(SetModeTok::Other);
        }
    }
    toks
}

fn is_session_sql_mode_name(tok: &SetModeTok) -> bool {
    match tok {
        SetModeTok::Word(w) | SetModeTok::Backtick(w) => w.eq_ignore_ascii_case("SQL_MODE"),
        _ => false,
    }
}

fn is_session_sql_mode_sysvar(rhs: &[SetModeTok]) -> bool {
    match rhs {
        [SetModeTok::DoubleAt, t] => is_session_sql_mode_name(t),
        [
            SetModeTok::DoubleAt,
            SetModeTok::Word(scope),
            SetModeTok::Dot,
            t,
        ] if scope == "SESSION" || scope == "LOCAL" => is_session_sql_mode_name(t),
        _ => false,
    }
}

fn split_top_level_commas(toks: &[SetModeTok]) -> Vec<&[SetModeTok]> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut depth = 0usize;
    for (idx, tok) in toks.iter().enumerate() {
        match tok {
            SetModeTok::LParen => depth += 1,
            SetModeTok::RParen => depth = depth.saturating_sub(1),
            SetModeTok::Comma if depth == 0 => {
                parts.push(&toks[start..idx]);
                start = idx + 1;
            }
            _ => {}
        }
    }
    parts.push(&toks[start..]);
    parts
}

const MAX_SQL_MODE_EXPR_DEPTH: usize = 32;
const MAX_SQL_MODE_EXPR_STEPS: usize = 256;
const MAX_SQL_MODE_EXPR_BYTES: usize = 8 * 1024;

fn strip_wrapping_parens(rhs: &[SetModeTok]) -> &[SetModeTok] {
    let leading = rhs.iter().take_while(|t| matches!(t, SetModeTok::LParen)).count();
    let trailing = rhs.iter().rev().take_while(|t| matches!(t, SetModeTok::RParen)).count();
    let max_pairs = leading.min(trailing);
    if max_pairs == 0 || max_pairs * 2 >= rhs.len() {
        return rhs;
    }
    let mut depth = max_pairs;
    let mut min_depth = max_pairs;
    for tok in &rhs[max_pairs..rhs.len() - max_pairs] {
        match tok {
            SetModeTok::LParen => depth += 1,
            SetModeTok::RParen => {
                if depth == 0 {
                    return rhs;
                }
                depth -= 1;
                min_depth = min_depth.min(depth);
            }
            _ => {}
        }
    }
    if depth != max_pairs {
        return rhs;
    }
    &rhs[min_depth..rhs.len() - min_depth]
}

fn eval_mysql_sql_mode_expr(rhs: &[SetModeTok], current_mode: MysqlQuoteMode) -> Option<String> {
    let mut steps = 0usize;
    eval_mysql_sql_mode_expr_depth(rhs, current_mode, 0, &mut steps)
}

fn eval_mysql_sql_mode_expr_depth(
    rhs: &[SetModeTok],
    current_mode: MysqlQuoteMode,
    depth: usize,
    steps: &mut usize,
) -> Option<String> {
    *steps += 1;
    if depth >= MAX_SQL_MODE_EXPR_DEPTH || *steps > MAX_SQL_MODE_EXPR_STEPS {
        return None;
    }
    let rhs = strip_wrapping_parens(rhs);
    if rhs.is_empty() {
        return None;
    }
    if rhs.iter().all(|t| matches!(t, SetModeTok::Str(_))) {
        let mut total_len = 0usize;
        for tok in rhs {
            if let SetModeTok::Str(s) = tok {
                total_len = total_len.checked_add(s.len())?;
                if total_len > MAX_SQL_MODE_EXPR_BYTES {
                    return None;
                }
            }
        }
        let mut out = String::with_capacity(total_len);
        for tok in rhs {
            if let SetModeTok::Str(s) = tok {
                out.push_str(s);
            }
        }
        return Some(out);
    }
    if is_session_sql_mode_sysvar(rhs) {
        return Some(current_mode.as_sql_mode_string().to_owned());
    }
    if let [SetModeTok::Word(fn_name), SetModeTok::LParen, inner @ .., SetModeTok::RParen] = rhs
        && (fn_name == "CONCAT" || fn_name == "CONCAT_WS")
    {
        let args = split_top_level_commas(inner);
        let mut vals = Vec::with_capacity(args.len());
        for arg in args {
            vals.push(eval_mysql_sql_mode_expr_depth(
                arg,
                current_mode,
                depth + 1,
                steps,
            )?);
        }
        if fn_name == "CONCAT" {
            let mut total_len = 0usize;
            for v in &vals {
                total_len = total_len.checked_add(v.len())?;
                if total_len > MAX_SQL_MODE_EXPR_BYTES {
                    return None;
                }
            }
            let mut out = String::with_capacity(total_len);
            for v in vals {
                out.push_str(&v);
            }
            return Some(out);
        }
        let (sep, rest) = vals.split_first()?;
        let mut total_len = 0usize;
        for (pos, v) in rest.iter().enumerate() {
            let sep_len = if pos > 0 { sep.len() } else { 0 };
            total_len = total_len.checked_add(sep_len)?.checked_add(v.len())?;
            if total_len > MAX_SQL_MODE_EXPR_BYTES {
                return None;
            }
        }
        let mut out = String::with_capacity(total_len);
        for (pos, v) in rest.iter().enumerate() {
            if pos > 0 {
                out.push_str(sep);
            }
            out.push_str(v);
        }
        return Some(out);
    }
    None
}

fn set_statement_assigns_sql_mode(assigns: &[SetModeTok]) -> bool {
    for assign in split_top_level_commas(assigns) {
        let mut depth = 0usize;
        let eq_pos = assign.iter().position(|tok| match tok {
            SetModeTok::LParen => {
                depth += 1;
                false
            }
            SetModeTok::RParen => {
                depth = depth.saturating_sub(1);
                false
            }
            SetModeTok::Eq if depth == 0 => true,
            _ => false,
        });
        let Some(eq_idx) = eq_pos else {
            continue;
        };
        let lhs = &assign[..eq_idx];
        let matches_sql_mode = match lhs {
            [t] => is_session_sql_mode_name(t),
            [SetModeTok::Word(scope), t] if scope == "SESSION" || scope == "LOCAL" => {
                is_session_sql_mode_name(t)
            }
            [SetModeTok::DoubleAt, t] => is_session_sql_mode_name(t),
            [
                SetModeTok::DoubleAt,
                SetModeTok::Word(scope),
                SetModeTok::Dot,
                t,
            ] if scope == "SESSION" || scope == "LOCAL" => is_session_sql_mode_name(t),
            _ => false,
        };
        if matches_sql_mode {
            return true;
        }
    }
    false
}

fn strip_mariadb_set_mode_statement_prefix(mut toks: &[SetModeTok]) -> (&[SetModeTok], bool) {
    let mut outer_sets_sql_mode = false;
    while matches!(
        toks,
        [SetModeTok::Word(a), SetModeTok::Word(b), ..] if a == "SET" && b == "STATEMENT"
    ) {
        let mut depth = 0usize;
        let Some(for_idx) = toks[2..].iter().position(|tok| match tok {
            SetModeTok::LParen => {
                depth += 1;
                false
            }
            SetModeTok::RParen => {
                depth = depth.saturating_sub(1);
                false
            }
            SetModeTok::Word(w) if depth == 0 && w == "FOR" => true,
            _ => false,
        }) else {
            return (toks, outer_sets_sql_mode);
        };
        if set_statement_assigns_sql_mode(&toks[2..2 + for_idx]) {
            outer_sets_sql_mode = true;
        }
        toks = &toks[2 + for_idx + 1..];
    }
    (toks, outer_sets_sql_mode)
}

fn strip_leading_block_label_set_mode(toks: &[SetModeTok]) -> &[SetModeTok] {
    if let [
        SetModeTok::Word(_) | SetModeTok::Backtick(_),
        SetModeTok::Colon,
        rest @ ..,
    ] = toks
    {
        rest
    } else {
        toks
    }
}

fn parse_inline_mysql_set_sql_mode_unwrapped(
    unwrapped: &str,
    current_mode: MysqlQuoteMode,
) -> Option<Option<MysqlQuoteMode>> {
    let raw_toks = tokenize_mysql_set_sql_mode(unwrapped, current_mode);
    let (toks, outer_sets_sql_mode) = strip_mariadb_set_mode_statement_prefix(&raw_toks);
    if outer_sets_sql_mode {
        // MariaDB SET STATEMENT sql_mode=... FOR <stmt> saves sql_mode before the inner
        // statement runs and restores current_mode when it exits (R18-5).
        return None;
    }
    if matches!(
        toks.first(),
        Some(SetModeTok::Word(w)) if w == "EXECUTE" || w == "CALL"
    ) {
        return Some(None);
    }
    let compound_toks = strip_leading_block_label_set_mode(toks);
    if matches!(
        compound_toks.first(),
        Some(SetModeTok::Word(w))
            if matches!(w.as_str(), "BEGIN" | "IF" | "CASE" | "LOOP" | "WHILE" | "REPEAT" | "FOR")
    ) && compound_toks.iter().any(|t| {
        matches!(t, SetModeTok::Word(w) if w == "EXECUTE" || w == "CALL")
            || is_session_sql_mode_name(t)
    }) {
        return Some(None);
    }
    if !matches!(toks.first(), Some(SetModeTok::Word(w)) if w == "SET")
        || matches!(
            toks,
            [SetModeTok::Word(a), SetModeTok::Word(b), ..] if a == "SET" && b == "STATEMENT"
        )
    {
        return None;
    }
    let mut transition: Option<Option<MysqlQuoteMode>> = None;
    // MySQL comma-separated SET inherits the most recent GLOBAL/PERSIST/SESSION/LOCAL modifier
    // for subsequent unqualified system variables while `@@scope.` and `@user` do not change it.
    let mut inherited_is_session = true;
    for assign in split_top_level_commas(&toks[1..]) {
        let mut depth = 0usize;
        let eq_pos = assign.iter().position(|tok| match tok {
            SetModeTok::LParen => {
                depth += 1;
                false
            }
            SetModeTok::RParen => {
                depth = depth.saturating_sub(1);
                false
            }
            SetModeTok::Eq if depth == 0 => true,
            _ => false,
        });
        let Some(eq_idx) = eq_pos else {
            continue;
        };
        let lhs = &assign[..eq_idx];
        let rhs = &assign[eq_idx + 1..];
        let is_session = match lhs {
            [SetModeTok::Word(scope), ..]
                if matches!(scope.as_str(), "GLOBAL" | "PERSIST" | "PERSIST_ONLY") =>
            {
                inherited_is_session = false;
                false
            }
            [SetModeTok::Word(scope), rest @ ..] if scope == "SESSION" || scope == "LOCAL" => {
                inherited_is_session = true;
                matches!(rest, [t] if is_session_sql_mode_name(t))
            }
            [t] => inherited_is_session && is_session_sql_mode_name(t),
            [SetModeTok::DoubleAt, t] => is_session_sql_mode_name(t),
            [
                SetModeTok::DoubleAt,
                SetModeTok::Word(scope),
                SetModeTok::Dot,
                t,
            ] if scope == "SESSION" || scope == "LOCAL" => is_session_sql_mode_name(t),
            _ => false,
        };
        if !is_session {
            continue;
        }
        let next = eval_mysql_sql_mode_expr(rhs, current_mode).map(|mode_str| {
            MysqlQuoteMode::from_sql_mode(&mode_str).with_server_version(current_mode.server_version)
        });
        transition = Some(next);
    }
    transition
}

/// Inspects a MySQL statement for inline `SET [SESSION|LOCAL] sql_mode` transitions.
///
/// When `current_mode.server_version` is known, gates resolve deterministically for that server.
/// Otherwise, if `conservative_on_conditional` is `true`, any conditional executable comment returns
/// `Some(None)` so safety and credential checks fail closed without mutating splitter expectations.
pub(crate) fn parse_inline_mysql_set_sql_mode(
    stmt_sql: &str,
    current_mode: MysqlQuoteMode,
    conservative_on_conditional: bool,
) -> Option<Option<MysqlQuoteMode>> {
    if !mysql_stmt_may_touch_sql_mode_text(stmt_sql) {
        return None;
    }
    if let Some(sv) = current_mode.server_version {
        let mut combined: Option<Option<Option<MysqlQuoteMode>>> = None;
        for policy in mysql_policies_for_known_server(stmt_sql, sv) {
            let (unwrapped, ended_in_quote, _) =
                unwrap_mysql_executable_comments_with_policy(stmt_sql, current_mode, policy);
            let res = if ended_in_quote {
                None
            } else {
                parse_inline_mysql_set_sql_mode_unwrapped(&unwrapped, current_mode)
            };
            match combined {
                None => combined = Some(res),
                Some(prev) if prev == res => {}
                Some(_) => return Some(None),
            }
        }
        return combined.unwrap_or(None);
    }
    let (uncond_sql, uncond_in_quote, saw_conditional) = unwrap_mysql_executable_comments_with_policy(
        stmt_sql,
        current_mode,
        MysqlCommentPolicy::UnconditionalOnly,
    );
    if saw_conditional && conservative_on_conditional {
        return Some(None);
    }
    if uncond_in_quote {
        None
    } else {
        parse_inline_mysql_set_sql_mode_unwrapped(&uncond_sql, current_mode)
    }
}

fn mysql_statement_parses_in_mode(stmt_sql: &str, mode: MysqlQuoteMode) -> bool {
    let (unwrapped, ended_in_quote) = unwrap_mysql_executable_comments_with_mode(stmt_sql, mode);
    if ended_in_quote {
        return false;
    }
    let normalized = mysql_normalize_for_parser(&unwrapped, mode);
    Parser::parse_sql(&MySqlDialect {}, &normalized).is_ok()
}

fn mysql_unparsed_count(
    stmts: &[SqlStatement],
    initial_mode: MysqlQuoteMode,
    track_inline_sql_mode: bool,
) -> usize {
    let mut active = Some(initial_mode);
    let mut unparsed = 0usize;
    for stmt in stmts {
        match active {
            Some(mode) => {
                if !mysql_statement_parses_in_mode(&stmt.sql, mode) {
                    unparsed += 1;
                }
                if track_inline_sql_mode {
                    match parse_inline_mysql_set_sql_mode(&stmt.sql, mode, false) {
                        Some(Some(next)) => active = Some(next),
                        Some(None) => active = None,
                        None => {}
                    }
                }
            }
            None => {
                let mut candidates = vec![
                    MysqlQuoteMode::DEFAULT.with_server_version(initial_mode.server_version),
                    MysqlQuoteMode::ANSI_QUOTES.with_server_version(initial_mode.server_version),
                    MysqlQuoteMode::NO_BACKSLASH_ESCAPES
                        .with_server_version(initial_mode.server_version),
                    MysqlQuoteMode::ANSI_AND_NO_BACKSLASH
                        .with_server_version(initial_mode.server_version),
                ];
                if stmt.sql.contains('[') {
                    candidates.push(
                        MysqlQuoteMode::MSSQL.with_server_version(initial_mode.server_version),
                    );
                    candidates.push(
                        MysqlQuoteMode::MSSQL_AND_NO_BACKSLASH
                            .with_server_version(initial_mode.server_version),
                    );
                }
                let any_ok = candidates
                    .into_iter()
                    .any(|m| mysql_statement_parses_in_mode(&stmt.sql, m));
                if !any_ok {
                    unparsed += 1;
                }
            }
        }
    }
    unparsed
}

fn is_pg_disallowed_col_id(word: &str) -> bool {
    matches!(
        word.to_ascii_uppercase().as_str(),
        "ALL"
            | "ANALYSE"
            | "ANALYZE"
            | "AND"
            | "ANY"
            | "ARRAY"
            | "AS"
            | "ASC"
            | "ASYMMETRIC"
            | "AUTHORIZATION"
            | "BINARY"
            | "BOTH"
            | "CASE"
            | "CAST"
            | "CHECK"
            | "COLLATE"
            | "COLLATION"
            | "COLUMN"
            | "CONCURRENTLY"
            | "CONSTRAINT"
            | "CREATE"
            | "CROSS"
            | "CURRENT_CATALOG"
            | "CURRENT_DATE"
            | "CURRENT_ROLE"
            | "CURRENT_SCHEMA"
            | "CURRENT_TIME"
            | "CURRENT_TIMESTAMP"
            | "CURRENT_USER"
            | "DEFAULT"
            | "DEFERRABLE"
            | "DESC"
            | "DISTINCT"
            | "DO"
            | "ELSE"
            | "END"
            | "EXCEPT"
            | "FALSE"
            | "FETCH"
            | "FOR"
            | "FOREIGN"
            | "FREEZE"
            | "FROM"
            | "FULL"
            | "GRANT"
            | "GROUP"
            | "HAVING"
            | "ILIKE"
            | "IN"
            | "INITIALLY"
            | "INNER"
            | "INTERSECT"
            | "INTO"
            | "IS"
            | "ISNULL"
            | "JOIN"
            | "LATERAL"
            | "LEADING"
            | "LEFT"
            | "LIKE"
            | "LIMIT"
            | "LOCALTIME"
            | "LOCALTIMESTAMP"
            | "NATURAL"
            | "NOT"
            | "NOTNULL"
            | "NULL"
            | "OFFSET"
            | "ON"
            | "ONLY"
            | "OR"
            | "ORDER"
            | "OUTER"
            | "OVERLAPS"
            | "PLACING"
            | "PRIMARY"
            | "REFERENCES"
            | "RETURNING"
            | "RIGHT"
            | "SELECT"
            | "SESSION_USER"
            | "SIMILAR"
            | "SOME"
            | "SYMMETRIC"
            | "SYSTEM_USER"
            | "TABLE"
            | "TABLESAMPLE"
            | "THEN"
            | "TO"
            | "TRAILING"
            | "TRUE"
            | "UNION"
            | "UNIQUE"
            | "USER"
            | "USING"
            | "VARIADIC"
            | "VERBOSE"
            | "WHEN"
            | "WHERE"
            | "WINDOW"
            | "WITH"
    )
}

fn pg_parse_statement_sql(stmt_sql: &str) -> Result<(), ParserError> {
    let dialect = PostgreSqlDialect {};
    let mut parser = Parser::new(&dialect).try_with_sql(stmt_sql)?;
    while parser.consume_token(&Token::SemiColon) {}
    if parser.peek_keyword(Keyword::TABLE) {
        let table_tok = parser.next_token();
        let only = parser.parse_keyword(Keyword::ONLY);
        let only_paren = only && parser.consume_token(&Token::LParen);
        let target_tok = parser.peek_token();
        let name = parser.parse_object_name(false)?;
        if !(1..=3).contains(&name.0.len()) {
            return parser.expected("table name", target_tok);
        }
        for (idx, part) in name.0.iter().enumerate() {
            let Some(ident) = part.as_ident() else {
                return parser.expected("table name", target_tok);
            };
            if ident.value.is_empty()
                || (idx == 0 && ident.quote_style.is_none() && is_pg_disallowed_col_id(&ident.value))
            {
                return parser.expected("table name", target_tok);
            }
        }
        if only_paren {
            parser.expect_token(&Token::RParen)?;
        } else if !only {
            let _has_star = parser.consume_token(&Token::Mul);
        }
        let rest_idx = parser.index();
        let all_tokens = parser.into_tokens();
        let rest = all_tokens.get(rest_idx..).unwrap_or(&[]);

        let mut normalized_tokens = Vec::with_capacity(rest.len() + 4);
        normalized_tokens.push(table_tok);
        normalized_tokens.push(TokenWithSpan::wrap(Token::Whitespace(Whitespace::Space)));
        normalized_tokens.push(TokenWithSpan::wrap(Token::Word(Word {
            value: "t".to_owned(),
            quote_style: Some('"'),
            keyword: Keyword::NoKeyword,
        })));
        normalized_tokens.push(TokenWithSpan::wrap(Token::Whitespace(Whitespace::Space)));
        normalized_tokens.extend_from_slice(rest);

        let mut query_parser = Parser::new(&dialect).with_tokens_with_locations(normalized_tokens);
        query_parser.parse_query()?;
        while query_parser.consume_token(&Token::SemiColon) {}
        if query_parser.peek_token_ref().token != Token::EOF {
            return query_parser.expected_ref("end of statement", query_parser.peek_token_ref());
        }
        Ok(())
    } else {
        parser.parse_statements().map(|_| ())
    }
}

fn pg_parse_statement_in_scs(stmt_sql: &str, scs: bool) -> Result<(), ParserError> {
    if scs {
        let normalized = pg_normalize_byte_escapes_for_parser(stmt_sql);
        pg_parse_statement_sql(normalized.as_ref())
    } else {
        let legacy = pg_normalize_legacy_escape_strings(stmt_sql);
        let normalized = pg_normalize_byte_escapes_for_parser(legacy.as_ref());
        pg_parse_statement_sql(normalized.as_ref())
    }
}

fn is_sqlparser_eof_error(err: &ParserError) -> bool {
    match err {
        ParserError::ParserError(msg) => {
            msg.ends_with("found: EOF")
                || msg == "Empty input when parsing identifier"
                || msg == "Trailing period in identifier"
        }
        ParserError::TokenizerError(msg) => {
            msg.contains("Unexpected EOF") || msg.contains("before EOF")
        }
        ParserError::RecursionLimitExceeded => false,
    }
}

fn pg_statement_parses_in_scs(stmt_sql: &str, scs: bool) -> bool {
    pg_parse_statement_in_scs(stmt_sql, scs).is_ok()
}

fn pg_closed_tail_parses_in_scs(stmt_sql: &str, scs: bool) -> bool {
    match pg_parse_statement_in_scs(stmt_sql, scs) {
        Ok(()) => true,
        Err(err) => is_sqlparser_eof_error(&err),
    }
}

/// Splits PostgreSQL SQL text on top-level `;` while respecting quotes, `--` comments,
/// nested `/* ... */` comments, and `$tag$...$tag$` dollar-quoted blocks.
pub fn split_statements(input: &str) -> Vec<SqlStatement> {
    split_statements_for_engine(input, DbEngine::Postgres)
}

/// Splits SQL text on top-level `;` using dialect-specific quoting and comment rules.
pub fn split_statements_for_engine(input: &str, engine: DbEngine) -> Vec<SqlStatement> {
    split_statements_for_engine_with_mysql_mode(input, engine, None)
}

fn extend_shifted_statements(
    out: &mut Vec<SqlStatement>,
    mut stmts: Vec<SqlStatement>,
    base_offset: usize,
) {
    if base_offset > 0 {
        for s in &mut stmts {
            s.start_byte += base_offset;
            s.end_byte += base_offset;
        }
    }
    out.extend(stmts);
}

/// Splits SQL text on top-level `;` using `engine` and an optional live MySQL [`MysqlQuoteMode`].
pub fn split_statements_for_engine_with_mysql_mode(
    input: &str,
    engine: DbEngine,
    mysql_mode: Option<MysqlQuoteMode>,
) -> Vec<SqlStatement> {
    split_statements_for_engine_with_session_mode(
        input,
        engine,
        SqlSessionMode::mysql(mysql_mode),
    )
}

/// Splits SQL text on top-level `;` using `engine` and live [`SqlSessionMode`] flags.
pub fn split_statements_for_engine_with_session_mode(
    input: &str,
    engine: DbEngine,
    session_mode: SqlSessionMode,
) -> Vec<SqlStatement> {
    split_statements_for_engine_with_session_opts(input, engine, session_mode, true)
}

const MAX_PG_FALLBACK_SCORE_DEPTH: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct PgFallbackScore {
    unparsed: usize,
    unclosed: bool,
    unclosed_tail_len: usize,
}

fn single_pg_scan_score(
    slice_len: usize,
    scan: &SplitScanResult,
    start_mode: &PostgresSessionMode,
    track_inline_sql_mode: bool,
) -> PgFallbackScore {
    let (delimited_stmts, undelimited_tail_stmt) =
        if scan.tail_undelimited && !scan.statements.is_empty() {
            let complete_len = scan.statements.len() - 1;
            (&scan.statements[..complete_len], Some(&scan.statements[complete_len]))
        } else {
            (scan.statements.as_slice(), None)
        };
    let (unclosed, unclosed_tail_len, closing_suffix) = match scan.pg_unfinished_tail {
        Some(ref tail) => (
            true,
            slice_len.saturating_sub(tail.start_byte),
            Some(tail.closing_suffix.as_str()),
        ),
        None => (scan.unclosed, 0, None),
    };

    let mut tracker = PgScsTracker::from_mode(start_mode);
    let mut unparsed = 0usize;
    for stmt in delimited_stmts {
        let parses = match tracker.active {
            Some(scs) => pg_statement_parses_in_scs(&stmt.sql, scs),
            None => {
                pg_statement_parses_in_scs(&stmt.sql, true)
                    || pg_statement_parses_in_scs(&stmt.sql, false)
            }
        };
        if !parses {
            unparsed += 1;
        }
        if track_inline_sql_mode {
            tracker.apply(&stmt.sql);
        }
    }

    if let Some(tail_stmt) = undelimited_tail_stmt {
        let closed_buf = closing_suffix.map(|suffix| format!("{}{}", tail_stmt.sql, suffix));
        let tail_sql = closed_buf.as_deref().unwrap_or(tail_stmt.sql.as_str());
        let parses = match tracker.active {
            Some(scs) => pg_closed_tail_parses_in_scs(tail_sql, scs),
            None => {
                pg_closed_tail_parses_in_scs(tail_sql, true)
                    || pg_closed_tail_parses_in_scs(tail_sql, false)
            }
        };
        if !parses {
            unparsed += 1;
        }
    }

    PgFallbackScore {
        unparsed,
        unclosed,
        unclosed_tail_len,
    }
}

fn score_pg_candidate_to_end(
    slice: &str,
    scan: &SplitScanResult,
    start_mode: &PostgresSessionMode,
    track_inline_sql_mode: bool,
    depth: usize,
) -> PgFallbackScore {
    let mut score = single_pg_scan_score(slice.len(), scan, start_mode, track_inline_sql_mode);
    let Some(mut pos) = scan.dyn_offset else {
        return score;
    };
    if pos >= slice.len() {
        return score;
    }
    let Some(mut carried) = scan.next_pg_mode.clone() else {
        return score;
    };

    while pos < slice.len() {
        let sub = &slice[pos..];
        let mut mode_on = carried.clone();
        mode_on.standard_conforming_strings = true;
        let mut chosen = split_statements_with_opts(
            sub,
            DbEngine::Postgres,
            MysqlQuoteMode::DEFAULT,
            &mode_on,
            track_inline_sql_mode,
            false,
        );
        let chosen_mode = mode_on;
        let mut chosen_score =
            single_pg_scan_score(sub.len(), &chosen, &chosen_mode, track_inline_sql_mode);

        if chosen.unclosed || chosen.saw_escaped_quote {
            let mut mode_off = carried.clone();
            mode_off.standard_conforming_strings = false;
            let off = split_statements_with_opts(
                sub,
                DbEngine::Postgres,
                MysqlQuoteMode::DEFAULT,
                &mode_off,
                track_inline_sql_mode,
                false,
            );
            let chosen_end = chosen.dyn_offset.unwrap_or(sub.len());
            let off_end = off.dyn_offset.unwrap_or(sub.len());
            if chosen_end == off_end && chosen.next_pg_mode == off.next_pg_mode {
                let off_score =
                    single_pg_scan_score(sub.len(), &off, &mode_off, track_inline_sql_mode);
                if off.statements != chosen.statements && off_score < chosen_score {
                    chosen = off;
                    chosen_score = off_score;
                }
            } else if depth < MAX_PG_FALLBACK_SCORE_DEPTH {
                let on_total = score_pg_candidate_to_end(
                    sub,
                    &chosen,
                    &chosen_mode,
                    track_inline_sql_mode,
                    depth + 1,
                );
                let off_total = score_pg_candidate_to_end(
                    sub,
                    &off,
                    &mode_off,
                    track_inline_sql_mode,
                    depth + 1,
                );
                let best_tail = on_total.min(off_total);
                return PgFallbackScore {
                    unparsed: score.unparsed + best_tail.unparsed,
                    unclosed: score.unclosed || best_tail.unclosed,
                    unclosed_tail_len: if best_tail.unclosed {
                        best_tail.unclosed_tail_len
                    } else {
                        score.unclosed_tail_len
                    },
                };
            } else {
                let off_score =
                    single_pg_scan_score(sub.len(), &off, &mode_off, track_inline_sql_mode);
                if off.statements != chosen.statements && off_score < chosen_score {
                    chosen = off;
                    chosen_score = off_score;
                }
            }
        }

        score.unparsed += chosen_score.unparsed;
        score.unclosed |= chosen_score.unclosed;
        if chosen_score.unclosed {
            score.unclosed_tail_len = chosen_score.unclosed_tail_len;
        }
        if let Some(rel) = chosen.dyn_offset
            && rel > 0
            && pos + rel < slice.len()
        {
            pos += rel;
            if let Some(next_mode) = chosen.next_pg_mode {
                carried = next_mode;
            }
            continue;
        }
        break;
    }

    score
}

fn split_statements_for_engine_with_session_opts(
    input: &str,
    engine: DbEngine,
    session_mode: SqlSessionMode,
    track_inline_sql_mode: bool,
) -> Vec<SqlStatement> {
    let default_pg_mode = PostgresSessionMode::new(true);
    if engine == DbEngine::Sqlite {
        return split_statements_with_opts(
            input,
            engine,
            MysqlQuoteMode::DEFAULT,
            &default_pg_mode,
            track_inline_sql_mode,
            true,
        )
        .statements;
    }

    if engine == DbEngine::Postgres {
        let initial_pg = session_mode.pg_session_mode();
        let mut out = Vec::new();
        let mut cur_offset = 0usize;
        let mut cur_pg_mode = Some(initial_pg.clone());
        let mut carried_pg_mode = initial_pg;

        while cur_offset < input.len() {
            let slice = &input[cur_offset..];
            if let Some(ref pg_mode) = cur_pg_mode {
                let scan = split_statements_with_opts(
                    slice,
                    DbEngine::Postgres,
                    MysqlQuoteMode::DEFAULT,
                    pg_mode,
                    track_inline_sql_mode,
                    true,
                );
                extend_shifted_statements(&mut out, scan.statements, cur_offset);
                if let Some(rel) = scan.dyn_offset
                    && rel > 0
                    && cur_offset + rel < input.len()
                {
                    cur_offset += rel;
                    cur_pg_mode = None;
                    if let Some(next_mode) = scan.next_pg_mode {
                        carried_pg_mode = next_mode;
                    }
                    continue;
                }
                break;
            }

            let mut mode_on = carried_pg_mode.clone();
            mode_on.standard_conforming_strings = true;
            let mut best = split_statements_with_opts(
                slice,
                DbEngine::Postgres,
                MysqlQuoteMode::DEFAULT,
                &mode_on,
                track_inline_sql_mode,
                false,
            );
            if best.unclosed || best.saw_escaped_quote {
                let mut mode_off = carried_pg_mode.clone();
                mode_off.standard_conforming_strings = false;
                let off = split_statements_with_opts(
                    slice,
                    DbEngine::Postgres,
                    MysqlQuoteMode::DEFAULT,
                    &mode_off,
                    track_inline_sql_mode,
                    false,
                );
                let best_end = best.dyn_offset.unwrap_or(slice.len());
                let off_end = off.dyn_offset.unwrap_or(slice.len());
                let prefer_off = if best_end == off_end && best.next_pg_mode == off.next_pg_mode {
                    off.statements != best.statements
                        && single_pg_scan_score(slice.len(), &off, &mode_off, track_inline_sql_mode)
                            < single_pg_scan_score(slice.len(), &best, &mode_on, track_inline_sql_mode)
                } else {
                    score_pg_candidate_to_end(slice, &off, &mode_off, track_inline_sql_mode, 0)
                        < score_pg_candidate_to_end(slice, &best, &mode_on, track_inline_sql_mode, 0)
                };
                if prefer_off {
                    best = off;
                }
            }

            extend_shifted_statements(&mut out, best.statements, cur_offset);
            if let Some(rel) = best.dyn_offset
                && rel > 0
                && cur_offset + rel < input.len()
            {
                cur_offset += rel;
                if let Some(next_mode) = best.next_pg_mode {
                    carried_pg_mode = next_mode;
                }
                continue;
            }
            break;
        }

        return out;
    }

    let mysql_mode = session_mode.mysql;
    let server_version = mysql_mode.and_then(|m| m.server_version);
    let default_mode = MysqlQuoteMode::DEFAULT.with_server_version(server_version);
    let mut out = Vec::new();
    let mut cur_offset = 0usize;
    let mut cur_mode = mysql_mode;

    while cur_offset < input.len() {
        let slice = &input[cur_offset..];
        if let Some(mode) = cur_mode {
            let scan = split_statements_with_opts(
                slice,
                DbEngine::Mysql,
                mode,
                &default_pg_mode,
                track_inline_sql_mode,
                true,
            );
            extend_shifted_statements(&mut out, scan.statements, cur_offset);
            if let Some(rel) = scan.dyn_offset
                && rel > 0
                && cur_offset + rel < input.len()
            {
                cur_offset += rel;
                cur_mode = None;
                continue;
            }
            break;
        }

        let mut best = split_statements_with_opts(
            slice,
            DbEngine::Mysql,
            default_mode,
            &default_pg_mode,
            track_inline_sql_mode,
            true,
        );
        let has_brackets = slice.contains('[');
        if best.unclosed || best.saw_escaped_quote || has_brackets {
            let mut best_mode = default_mode;
            let mut best_unparsed: Option<usize> = None;
            let mut candidate_modes = vec![
                MysqlQuoteMode::ANSI_QUOTES,
                MysqlQuoteMode::NO_BACKSLASH_ESCAPES,
                MysqlQuoteMode::ANSI_AND_NO_BACKSLASH,
            ];
            if has_brackets {
                candidate_modes.push(MysqlQuoteMode::MSSQL);
                candidate_modes.push(MysqlQuoteMode::MSSQL_AND_NO_BACKSLASH);
            }
            for base_mode in candidate_modes {
                let mode = base_mode.with_server_version(server_version);
                let cand = split_statements_with_opts(
                    slice,
                    DbEngine::Mysql,
                    mode,
                    &default_pg_mode,
                    track_inline_sql_mode,
                    true,
                );
                if cand.unclosed {
                    continue;
                }
                if best.unclosed {
                    best = cand;
                    best_mode = mode;
                    best_unparsed = None;
                    continue;
                }
                if cand.statements != best.statements || !best_mode.quote_flags_eq(MysqlQuoteMode::DEFAULT) {
                    let base_unparsed = *best_unparsed.get_or_insert_with(|| {
                        mysql_unparsed_count(&best.statements, best_mode, track_inline_sql_mode)
                    });
                    let cand_unparsed =
                        mysql_unparsed_count(&cand.statements, mode, track_inline_sql_mode);
                    if cand_unparsed < base_unparsed {
                        best = cand;
                        best_mode = mode;
                        best_unparsed = Some(cand_unparsed);
                    }
                }
            }
        }

        extend_shifted_statements(&mut out, best.statements, cur_offset);
        if let Some(rel) = best.dyn_offset
            && rel > 0
            && cur_offset + rel < input.len()
        {
            cur_offset += rel;
            continue;
        }
        break;
    }

    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MysqlCompoundKind {
    Begin,
    If,
    Case,
    Loop,
    While,
    Repeat,
    For,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MysqlClauseKw {
    Then,
    Do,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MysqlInnerBeginState {
    None,
    AfterBegin,
    AfterBeginNot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MysqlDeclareHandlerState {
    None,
    AfterDeclare,
    AfterAction,
    AfterHandler,
    ExpectCondStart,
    AfterSqlstate,
    AfterSqlstateValue,
    AfterNot,
    AfterCondItem,
    ReadingCondDigits,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MysqlTopStartState {
    Start,
    AfterLeadingIdent { was_begin: bool, was_set: bool },
    AfterBeginNot,
    InSetStatementHeader,
    NotCompound,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MysqlInnerSetStatementState {
    None,
    AfterSet,
    InHeader,
}

#[derive(Clone, Debug)]
struct MysqlCompoundTracker {
    stack: Vec<MysqlCompoundKind>,
    top_state: MysqlTopStartState,
    saw_top_label: bool,
    at_body_stmt_start: bool,
    inner_begin: MysqlInnerBeginState,
    inner_set_stmt: MysqlInnerSetStatementState,
    pending_inner_label: bool,
    pending_end_kind: Option<MysqlCompoundKind>,
    waiting_clause_kw: Option<MysqlClauseKw>,
    expr_case_depth: usize,
    in_repeat_until: bool,
    declare_handler: MysqlDeclareHandlerState,
}

impl MysqlCompoundTracker {
    fn new() -> Self {
        Self {
            stack: Vec::new(),
            top_state: MysqlTopStartState::Start,
            saw_top_label: false,
            at_body_stmt_start: false,
            inner_begin: MysqlInnerBeginState::None,
            inner_set_stmt: MysqlInnerSetStatementState::None,
            pending_inner_label: false,
            pending_end_kind: None,
            waiting_clause_kw: None,
            expr_case_depth: 0,
            in_repeat_until: false,
            declare_handler: MysqlDeclareHandlerState::None,
        }
    }

    fn is_in_compound(&self) -> bool {
        !self.stack.is_empty()
    }

    fn reset(&mut self) {
        self.stack.clear();
        self.top_state = MysqlTopStartState::Start;
        self.saw_top_label = false;
        self.at_body_stmt_start = false;
        self.inner_begin = MysqlInnerBeginState::None;
        self.inner_set_stmt = MysqlInnerSetStatementState::None;
        self.pending_inner_label = false;
        self.pending_end_kind = None;
        self.waiting_clause_kw = None;
        self.expr_case_depth = 0;
        self.in_repeat_until = false;
        self.declare_handler = MysqlDeclareHandlerState::None;
    }

    fn on_whitespace(&mut self) {
        if self.declare_handler == MysqlDeclareHandlerState::ReadingCondDigits {
            self.declare_handler = MysqlDeclareHandlerState::AfterCondItem;
        }
    }

    fn on_semicolon(&mut self) {
        if !self.is_in_compound() {
            return;
        }
        self.inner_begin = MysqlInnerBeginState::None;
        self.inner_set_stmt = MysqlInnerSetStatementState::None;
        self.pending_inner_label = false;
        self.pending_end_kind = None;
        self.waiting_clause_kw = None;
        self.expr_case_depth = 0;
        self.in_repeat_until = false;
        self.declare_handler = MysqlDeclareHandlerState::None;
        self.at_body_stmt_start = true;
    }

    fn on_quoted_ident(&mut self, paren_depth: usize) {
        if paren_depth != 0 {
            return;
        }
        if !self.is_in_compound() {
            self.top_state = match self.top_state {
                MysqlTopStartState::Start => MysqlTopStartState::AfterLeadingIdent {
                    was_begin: false,
                    was_set: false,
                },
                MysqlTopStartState::InSetStatementHeader => MysqlTopStartState::InSetStatementHeader,
                _ => MysqlTopStartState::NotCompound,
            };
            return;
        }
        if self.inner_set_stmt == MysqlInnerSetStatementState::InHeader {
            return;
        }
        self.inner_set_stmt = MysqlInnerSetStatementState::None;
        self.pending_end_kind = None;
        match self.inner_begin {
            MysqlInnerBeginState::AfterBegin => {
                self.stack.push(MysqlCompoundKind::Begin);
                self.inner_begin = MysqlInnerBeginState::None;
                self.at_body_stmt_start = true;
            }
            MysqlInnerBeginState::AfterBeginNot => {
                self.inner_begin = MysqlInnerBeginState::None;
                self.at_body_stmt_start = false;
            }
            MysqlInnerBeginState::None => {}
        }
        match self.declare_handler {
            MysqlDeclareHandlerState::ExpectCondStart => {
                self.declare_handler = MysqlDeclareHandlerState::AfterCondItem;
                return;
            }
            MysqlDeclareHandlerState::AfterCondItem
            | MysqlDeclareHandlerState::ReadingCondDigits => {
                self.declare_handler = MysqlDeclareHandlerState::None;
                self.at_body_stmt_start = true;
            }
            MysqlDeclareHandlerState::None => {}
            _ => {
                self.declare_handler = MysqlDeclareHandlerState::None;
                return;
            }
        }
        if self.at_body_stmt_start {
            self.pending_inner_label = true;
            self.at_body_stmt_start = false;
        }
    }

    fn on_literal_or_non_colon_punct(&mut self, ch: Option<char>, paren_depth: usize) {
        if paren_depth != 0 {
            return;
        }
        if !self.is_in_compound() {
            if self.top_state == MysqlTopStartState::InSetStatementHeader {
                return;
            }
            if ch == Some(':')
                && matches!(self.top_state, MysqlTopStartState::AfterLeadingIdent { .. })
                && !self.saw_top_label
            {
                self.saw_top_label = true;
                self.top_state = MysqlTopStartState::Start;
            } else {
                self.top_state = MysqlTopStartState::NotCompound;
            }
            return;
        }
        if ch == Some(':') && self.pending_inner_label {
            self.pending_inner_label = false;
            self.inner_set_stmt = MysqlInnerSetStatementState::None;
            self.at_body_stmt_start = true;
            return;
        }
        if self.inner_set_stmt == MysqlInnerSetStatementState::InHeader {
            return;
        }
        self.inner_set_stmt = MysqlInnerSetStatementState::None;
        self.pending_inner_label = false;
        self.pending_end_kind = None;
        match self.declare_handler {
            MysqlDeclareHandlerState::AfterSqlstate
            | MysqlDeclareHandlerState::AfterSqlstateValue
                if ch.is_none() =>
            {
                self.declare_handler = MysqlDeclareHandlerState::AfterCondItem;
                return;
            }
            MysqlDeclareHandlerState::ExpectCondStart
                if ch.is_some_and(|c| c.is_ascii_digit()) =>
            {
                self.declare_handler = MysqlDeclareHandlerState::ReadingCondDigits;
                return;
            }
            MysqlDeclareHandlerState::ReadingCondDigits
                if ch.is_some_and(|c| c.is_ascii_digit()) =>
            {
                return;
            }
            MysqlDeclareHandlerState::AfterCondItem
            | MysqlDeclareHandlerState::ReadingCondDigits
                if ch == Some(',') =>
            {
                self.declare_handler = MysqlDeclareHandlerState::ExpectCondStart;
                return;
            }
            MysqlDeclareHandlerState::None => {}
            _ => {
                self.declare_handler = MysqlDeclareHandlerState::None;
            }
        }
        match self.inner_begin {
            MysqlInnerBeginState::AfterBegin => {
                self.stack.push(MysqlCompoundKind::Begin);
                self.inner_begin = MysqlInnerBeginState::None;
            }
            MysqlInnerBeginState::AfterBeginNot | MysqlInnerBeginState::None => {
                self.inner_begin = MysqlInnerBeginState::None;
            }
        }
        self.at_body_stmt_start = false;
    }

    fn on_word(&mut self, word: &str, paren_depth: usize) {
        if paren_depth != 0 {
            return;
        }
        if !self.is_in_compound() {
            match self.top_state {
                MysqlTopStartState::Start => {
                    if word.eq_ignore_ascii_case("BEGIN") {
                        self.top_state = MysqlTopStartState::AfterLeadingIdent {
                            was_begin: true,
                            was_set: false,
                        };
                    } else if !self.saw_top_label && word.eq_ignore_ascii_case("SET") {
                        self.top_state = MysqlTopStartState::AfterLeadingIdent {
                            was_begin: false,
                            was_set: true,
                        };
                    } else if !self.saw_top_label && word.eq_ignore_ascii_case("IF") {
                        self.stack.push(MysqlCompoundKind::If);
                        self.waiting_clause_kw = Some(MysqlClauseKw::Then);
                        self.at_body_stmt_start = false;
                        self.top_state = MysqlTopStartState::NotCompound;
                    } else if !self.saw_top_label && word.eq_ignore_ascii_case("CASE") {
                        self.stack.push(MysqlCompoundKind::Case);
                        self.waiting_clause_kw = Some(MysqlClauseKw::Then);
                        self.at_body_stmt_start = false;
                        self.top_state = MysqlTopStartState::NotCompound;
                    } else if word.eq_ignore_ascii_case("LOOP") {
                        self.stack.push(MysqlCompoundKind::Loop);
                        self.at_body_stmt_start = true;
                        self.top_state = MysqlTopStartState::NotCompound;
                    } else if word.eq_ignore_ascii_case("WHILE") {
                        self.stack.push(MysqlCompoundKind::While);
                        self.waiting_clause_kw = Some(MysqlClauseKw::Do);
                        self.at_body_stmt_start = false;
                        self.top_state = MysqlTopStartState::NotCompound;
                    } else if word.eq_ignore_ascii_case("REPEAT") {
                        self.stack.push(MysqlCompoundKind::Repeat);
                        self.at_body_stmt_start = true;
                        self.top_state = MysqlTopStartState::NotCompound;
                    } else if word.eq_ignore_ascii_case("FOR") {
                        self.stack.push(MysqlCompoundKind::For);
                        self.waiting_clause_kw = Some(MysqlClauseKw::Do);
                        self.at_body_stmt_start = false;
                        self.top_state = MysqlTopStartState::NotCompound;
                    } else {
                        self.top_state = MysqlTopStartState::AfterLeadingIdent {
                            was_begin: false,
                            was_set: false,
                        };
                    }
                }
                MysqlTopStartState::AfterLeadingIdent {
                    was_begin: true, ..
                } if word.eq_ignore_ascii_case("NOT") => {
                    self.top_state = MysqlTopStartState::AfterBeginNot;
                }
                MysqlTopStartState::AfterLeadingIdent {
                    was_set: true, ..
                } if !self.saw_top_label && word.eq_ignore_ascii_case("STATEMENT") => {
                    self.top_state = MysqlTopStartState::InSetStatementHeader;
                }
                MysqlTopStartState::AfterBeginNot if word.eq_ignore_ascii_case("ATOMIC") => {
                    self.stack.push(MysqlCompoundKind::Begin);
                    self.at_body_stmt_start = true;
                    self.top_state = MysqlTopStartState::NotCompound;
                }
                MysqlTopStartState::InSetStatementHeader => {
                    if word.eq_ignore_ascii_case("FOR") {
                        self.saw_top_label = false;
                        self.top_state = MysqlTopStartState::Start;
                    }
                }
                _ => {
                    self.top_state = MysqlTopStartState::NotCompound;
                }
            }
            return;
        }

        if let Some(kind) = self.pending_end_kind {
            self.pending_end_kind = None;
            let matches_kind = match kind {
                MysqlCompoundKind::If => word.eq_ignore_ascii_case("IF"),
                MysqlCompoundKind::Case => word.eq_ignore_ascii_case("CASE"),
                MysqlCompoundKind::Loop => word.eq_ignore_ascii_case("LOOP"),
                MysqlCompoundKind::While => word.eq_ignore_ascii_case("WHILE"),
                MysqlCompoundKind::Repeat => word.eq_ignore_ascii_case("REPEAT"),
                MysqlCompoundKind::For => word.eq_ignore_ascii_case("FOR"),
                MysqlCompoundKind::Begin => false,
            };
            if matches_kind {
                self.stack.pop();
                self.at_body_stmt_start = false;
                return;
            }
        }

        match self.inner_begin {
            MysqlInnerBeginState::AfterBegin => {
                if word.eq_ignore_ascii_case("WORK") {
                    self.inner_begin = MysqlInnerBeginState::None;
                    self.at_body_stmt_start = false;
                    return;
                }
                self.stack.push(MysqlCompoundKind::Begin);
                self.at_body_stmt_start = true;
                if word.eq_ignore_ascii_case("NOT") {
                    self.inner_begin = MysqlInnerBeginState::AfterBeginNot;
                    return;
                }
                self.inner_begin = MysqlInnerBeginState::None;
            }
            MysqlInnerBeginState::AfterBeginNot => {
                self.inner_begin = MysqlInnerBeginState::None;
                if word.eq_ignore_ascii_case("ATOMIC") {
                    self.at_body_stmt_start = true;
                    return;
                }
                self.at_body_stmt_start = false;
            }
            MysqlInnerBeginState::None => {}
        }

        match self.inner_set_stmt {
            MysqlInnerSetStatementState::AfterSet => {
                if word.eq_ignore_ascii_case("STATEMENT") {
                    self.inner_set_stmt = MysqlInnerSetStatementState::InHeader;
                    self.pending_inner_label = false;
                    return;
                }
                self.inner_set_stmt = MysqlInnerSetStatementState::None;
            }
            MysqlInnerSetStatementState::InHeader => {
                if word.eq_ignore_ascii_case("FOR") {
                    self.inner_set_stmt = MysqlInnerSetStatementState::None;
                    self.at_body_stmt_start = true;
                }
                return;
            }
            MysqlInnerSetStatementState::None => {}
        }

        self.pending_inner_label = false;

        match self.declare_handler {
            MysqlDeclareHandlerState::None => {}
            MysqlDeclareHandlerState::AfterDeclare => {
                self.declare_handler = if word.eq_ignore_ascii_case("CONTINUE")
                    || word.eq_ignore_ascii_case("EXIT")
                    || word.eq_ignore_ascii_case("UNDO")
                {
                    MysqlDeclareHandlerState::AfterAction
                } else {
                    MysqlDeclareHandlerState::None
                };
                return;
            }
            MysqlDeclareHandlerState::AfterAction => {
                self.declare_handler = if word.eq_ignore_ascii_case("HANDLER") {
                    MysqlDeclareHandlerState::AfterHandler
                } else {
                    MysqlDeclareHandlerState::None
                };
                return;
            }
            MysqlDeclareHandlerState::AfterHandler => {
                self.declare_handler = if word.eq_ignore_ascii_case("FOR") {
                    MysqlDeclareHandlerState::ExpectCondStart
                } else {
                    MysqlDeclareHandlerState::None
                };
                return;
            }
            MysqlDeclareHandlerState::ExpectCondStart => {
                self.declare_handler = if word.eq_ignore_ascii_case("SQLSTATE") {
                    MysqlDeclareHandlerState::AfterSqlstate
                } else if word.eq_ignore_ascii_case("NOT") {
                    MysqlDeclareHandlerState::AfterNot
                } else {
                    MysqlDeclareHandlerState::AfterCondItem
                };
                return;
            }
            MysqlDeclareHandlerState::AfterSqlstate => {
                self.declare_handler = if word.eq_ignore_ascii_case("VALUE") {
                    MysqlDeclareHandlerState::AfterSqlstateValue
                } else {
                    MysqlDeclareHandlerState::None
                };
                return;
            }
            MysqlDeclareHandlerState::AfterSqlstateValue => {
                self.declare_handler = MysqlDeclareHandlerState::None;
                return;
            }
            MysqlDeclareHandlerState::AfterNot => {
                self.declare_handler = if word.eq_ignore_ascii_case("FOUND") {
                    MysqlDeclareHandlerState::AfterCondItem
                } else {
                    MysqlDeclareHandlerState::None
                };
                return;
            }
            MysqlDeclareHandlerState::AfterCondItem
            | MysqlDeclareHandlerState::ReadingCondDigits => {
                self.declare_handler = MysqlDeclareHandlerState::None;
                self.at_body_stmt_start = true;
            }
        }

        if let Some(expected) = self.waiting_clause_kw {
            if word.eq_ignore_ascii_case("CASE") {
                self.expr_case_depth += 1;
                return;
            }
            if word.eq_ignore_ascii_case("END") && self.expr_case_depth > 0 {
                self.expr_case_depth -= 1;
                return;
            }
            let reached = self.expr_case_depth == 0
                && match expected {
                    MysqlClauseKw::Then => word.eq_ignore_ascii_case("THEN"),
                    MysqlClauseKw::Do => word.eq_ignore_ascii_case("DO"),
                };
            if reached {
                self.waiting_clause_kw = None;
                self.at_body_stmt_start = true;
            }
            return;
        }

        if self.in_repeat_until {
            if word.eq_ignore_ascii_case("CASE") {
                self.expr_case_depth += 1;
                return;
            }
            if word.eq_ignore_ascii_case("END") {
                if self.expr_case_depth > 0 {
                    self.expr_case_depth -= 1;
                } else {
                    self.in_repeat_until = false;
                    self.pending_end_kind = Some(MysqlCompoundKind::Repeat);
                    self.at_body_stmt_start = false;
                }
            }
            return;
        }

        if self.at_body_stmt_start {
            if word.eq_ignore_ascii_case("END") {
                match self.stack.last().copied() {
                    Some(MysqlCompoundKind::Begin) => {
                        self.stack.pop();
                    }
                    Some(kind) => {
                        self.pending_end_kind = Some(kind);
                    }
                    None => {}
                }
                self.at_body_stmt_start = false;
                return;
            }
            if word.eq_ignore_ascii_case("BEGIN") {
                self.inner_begin = MysqlInnerBeginState::AfterBegin;
                self.at_body_stmt_start = false;
                return;
            }
            if word.eq_ignore_ascii_case("IF") {
                self.stack.push(MysqlCompoundKind::If);
                self.waiting_clause_kw = Some(MysqlClauseKw::Then);
                self.at_body_stmt_start = false;
                return;
            }
            if word.eq_ignore_ascii_case("CASE") {
                self.stack.push(MysqlCompoundKind::Case);
                self.waiting_clause_kw = Some(MysqlClauseKw::Then);
                self.at_body_stmt_start = false;
                return;
            }
            if word.eq_ignore_ascii_case("LOOP") {
                self.stack.push(MysqlCompoundKind::Loop);
                self.at_body_stmt_start = true;
                return;
            }
            if word.eq_ignore_ascii_case("WHILE") {
                self.stack.push(MysqlCompoundKind::While);
                self.waiting_clause_kw = Some(MysqlClauseKw::Do);
                self.at_body_stmt_start = false;
                return;
            }
            if word.eq_ignore_ascii_case("REPEAT") {
                self.stack.push(MysqlCompoundKind::Repeat);
                self.at_body_stmt_start = true;
                return;
            }
            if word.eq_ignore_ascii_case("FOR") {
                self.stack.push(MysqlCompoundKind::For);
                self.waiting_clause_kw = Some(MysqlClauseKw::Do);
                self.at_body_stmt_start = false;
                return;
            }
            if word.eq_ignore_ascii_case("ELSEIF")
                && self.stack.last() == Some(&MysqlCompoundKind::If)
            {
                self.waiting_clause_kw = Some(MysqlClauseKw::Then);
                self.at_body_stmt_start = false;
                return;
            }
            if word.eq_ignore_ascii_case("WHEN")
                && self.stack.last() == Some(&MysqlCompoundKind::Case)
            {
                self.waiting_clause_kw = Some(MysqlClauseKw::Then);
                self.at_body_stmt_start = false;
                return;
            }
            if word.eq_ignore_ascii_case("ELSE")
                && matches!(
                    self.stack.last(),
                    Some(MysqlCompoundKind::If | MysqlCompoundKind::Case)
                )
            {
                return;
            }
            if word.eq_ignore_ascii_case("UNTIL")
                && self.stack.last() == Some(&MysqlCompoundKind::Repeat)
            {
                self.in_repeat_until = true;
                self.at_body_stmt_start = false;
                return;
            }
            if word.eq_ignore_ascii_case("DECLARE") {
                self.declare_handler = MysqlDeclareHandlerState::AfterDeclare;
                self.at_body_stmt_start = false;
                return;
            }
            if word.eq_ignore_ascii_case("SET") {
                self.inner_set_stmt = MysqlInnerSetStatementState::AfterSet;
            }
            self.pending_inner_label = true;
            self.at_body_stmt_start = false;
        }
    }
}

fn starts_as_mariadb_compound_stmt(sql: &str, mode: MysqlQuoteMode) -> bool {
    let bytes = sql.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;
    let mut state = ScanState::Normal;
    let mut tracker = MysqlCompoundTracker::new();
    let mut paren_depth = 0usize;
    let allow_sq_bs = mode.allow_single_quote_backslash();
    let allow_dq_bs = mode.allow_double_quote_backslash();
    let allow_bracket = mode.allow_bracket_quotes();

    while i < len {
        match state {
            ScanState::Normal => {
                let ch = sql[i..].chars().next().unwrap_or('\0');
                if bytes[i] == b'\'' {
                    tracker.on_literal_or_non_colon_punct(None, paren_depth);
                    state = ScanState::SingleQuote;
                    i += 1;
                } else if bytes[i] == b'"' {
                    if mode.ansi_quotes {
                        tracker.on_quoted_ident(paren_depth);
                    } else {
                        tracker.on_literal_or_non_colon_punct(None, paren_depth);
                    }
                    state = ScanState::DoubleQuote;
                    i += 1;
                } else if bytes[i] == b'`' {
                    tracker.on_quoted_ident(paren_depth);
                    state = ScanState::BacktickQuote;
                    i += 1;
                } else if allow_bracket && bytes[i] == b'[' {
                    tracker.on_quoted_ident(paren_depth);
                    state = ScanState::BracketQuote;
                    i += 1;
                } else if is_dash_comment_start(DbEngine::Mysql, sql, i) {
                    tracker.on_whitespace();
                    state = ScanState::LineComment;
                    i += 2;
                } else if bytes[i] == b'#' {
                    tracker.on_whitespace();
                    state = ScanState::LineComment;
                    i += 1;
                } else if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
                    tracker.on_whitespace();
                    state = ScanState::BlockComment(1);
                    i += 2;
                } else if bytes[i] == b';' {
                    return false;
                } else if is_ident_start_for_engine(DbEngine::Mysql, ch) {
                    let word_start = i;
                    i += ch.len_utf8();
                    while i < len {
                        let next_ch = sql[i..].chars().next().unwrap_or('\0');
                        if !is_ident_continue_for_engine(DbEngine::Mysql, next_ch) {
                            break;
                        }
                        i += next_ch.len_utf8();
                    }
                    tracker.on_word(&sql[word_start..i], paren_depth);
                    if tracker.is_in_compound() {
                        return true;
                    }
                    if tracker.top_state == MysqlTopStartState::NotCompound {
                        return false;
                    }
                } else if is_sql_whitespace(DbEngine::Mysql, ch) {
                    tracker.on_whitespace();
                    i += ch.len_utf8();
                } else if ch == ':' && i + 1 < len && bytes[i + 1] == b'=' {
                    tracker.on_literal_or_non_colon_punct(None, paren_depth);
                    if tracker.top_state == MysqlTopStartState::NotCompound {
                        return false;
                    }
                    i += 2;
                } else {
                    if ch == '(' {
                        tracker.on_literal_or_non_colon_punct(Some('('), paren_depth);
                        paren_depth += 1;
                    } else if ch == ')' {
                        paren_depth = paren_depth.saturating_sub(1);
                        tracker.on_literal_or_non_colon_punct(Some(')'), paren_depth);
                    } else {
                        tracker.on_literal_or_non_colon_punct(Some(ch), paren_depth);
                    }
                    if tracker.top_state == MysqlTopStartState::NotCompound {
                        return false;
                    }
                    i += ch.len_utf8();
                }
            }
            ScanState::SingleQuote => {
                if allow_sq_bs && bytes[i] == b'\\' {
                    i += 1;
                    if i < len {
                        i += char_len_at(sql, i);
                    }
                } else if bytes[i] == b'\'' {
                    if i + 1 < len && bytes[i + 1] == b'\'' {
                        i += 2;
                    } else {
                        state = ScanState::Normal;
                        i += 1;
                    }
                } else {
                    i += char_len_at(sql, i);
                }
            }
            ScanState::DoubleQuote => {
                if allow_dq_bs && bytes[i] == b'\\' {
                    i += 1;
                    if i < len {
                        i += char_len_at(sql, i);
                    }
                } else if bytes[i] == b'"' {
                    if i + 1 < len && bytes[i + 1] == b'"' {
                        i += 2;
                    } else {
                        state = ScanState::Normal;
                        i += 1;
                    }
                } else {
                    i += char_len_at(sql, i);
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
                    i += char_len_at(sql, i);
                }
            }
            ScanState::BracketQuote => {
                if bytes[i] == b']' {
                    if i + 1 < len && bytes[i + 1] == b']' {
                        i += 2;
                    } else {
                        state = ScanState::Normal;
                        i += 1;
                    }
                } else {
                    i += char_len_at(sql, i);
                }
            }
            ScanState::LineComment => {
                if bytes[i] == b'\n' {
                    state = ScanState::Normal;
                }
                i += char_len_at(sql, i);
            }
            ScanState::BlockComment(_) => {
                if i + 1 < len && bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    state = ScanState::Normal;
                    i += 2;
                } else {
                    i += char_len_at(sql, i);
                }
            }
            ScanState::EscapeSingleQuote | ScanState::PlainSingleQuote => {
                i += char_len_at(sql, i);
            }
        }
    }
    tracker.is_in_compound()
}

fn split_mysql_semicolons_ignoring_compound(sql: &str, mode: MysqlQuoteMode) -> Vec<String> {
    let bytes = sql.as_bytes();
    let len = bytes.len();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    let mut state = ScanState::Normal;
    let allow_sq_bs = mode.allow_single_quote_backslash();
    let allow_dq_bs = mode.allow_double_quote_backslash();
    let allow_bracket = mode.allow_bracket_quotes();

    while i < len {
        match state {
            ScanState::Normal => {
                if bytes[i] == b'\'' {
                    state = ScanState::SingleQuote;
                    i += 1;
                } else if bytes[i] == b'"' {
                    state = ScanState::DoubleQuote;
                    i += 1;
                } else if bytes[i] == b'`' {
                    state = ScanState::BacktickQuote;
                    i += 1;
                } else if allow_bracket && bytes[i] == b'[' {
                    state = ScanState::BracketQuote;
                    i += 1;
                } else if is_dash_comment_start(DbEngine::Mysql, sql, i) {
                    state = ScanState::LineComment;
                    i += 2;
                } else if bytes[i] == b'#' {
                    state = ScanState::LineComment;
                    i += 1;
                } else if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
                    state = ScanState::BlockComment(1);
                    i += 2;
                } else if bytes[i] == b';' {
                    let trimmed = trim_sql_whitespace_for_engine(DbEngine::Mysql, &sql[start..i]);
                    if !trimmed.is_empty() {
                        out.push(trimmed.to_owned());
                    }
                    i += 1;
                    start = i;
                } else {
                    i += char_len_at(sql, i);
                }
            }
            ScanState::SingleQuote => {
                if allow_sq_bs && bytes[i] == b'\\' {
                    i += 1;
                    if i < len {
                        i += char_len_at(sql, i);
                    }
                } else if bytes[i] == b'\'' {
                    if i + 1 < len && bytes[i + 1] == b'\'' {
                        i += 2;
                    } else {
                        state = ScanState::Normal;
                        i += 1;
                    }
                } else {
                    i += char_len_at(sql, i);
                }
            }
            ScanState::DoubleQuote => {
                if allow_dq_bs && bytes[i] == b'\\' {
                    i += 1;
                    if i < len {
                        i += char_len_at(sql, i);
                    }
                } else if bytes[i] == b'"' {
                    if i + 1 < len && bytes[i + 1] == b'"' {
                        i += 2;
                    } else {
                        state = ScanState::Normal;
                        i += 1;
                    }
                } else {
                    i += char_len_at(sql, i);
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
                    i += char_len_at(sql, i);
                }
            }
            ScanState::BracketQuote => {
                if bytes[i] == b']' {
                    if i + 1 < len && bytes[i + 1] == b']' {
                        i += 2;
                    } else {
                        state = ScanState::Normal;
                        i += 1;
                    }
                } else {
                    i += char_len_at(sql, i);
                }
            }
            ScanState::LineComment => {
                if bytes[i] == b'\n' {
                    state = ScanState::Normal;
                }
                i += char_len_at(sql, i);
            }
            ScanState::BlockComment(_) => {
                if i + 1 < len && bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    state = ScanState::Normal;
                    i += 2;
                } else {
                    i += char_len_at(sql, i);
                }
            }
            ScanState::EscapeSingleQuote | ScanState::PlainSingleQuote => {
                i += char_len_at(sql, i);
            }
        }
    }
    if start < len {
        let trimmed = trim_sql_whitespace_for_engine(DbEngine::Mysql, &sql[start..]);
        if !trimmed.is_empty() {
            out.push(trimmed.to_owned());
        }
    }
    out
}

fn find_mariadb_clause_kw_end(
    sql: &str,
    start: usize,
    mode: MysqlQuoteMode,
    target_kw: &str,
    stop_after_end_repeat: bool,
) -> Option<usize> {
    let bytes = sql.as_bytes();
    let len = bytes.len();
    let mut i = start;
    let mut paren_depth = 0usize;
    let mut expr_case_depth = 0usize;
    let mut pending_end_repeat = false;
    let allow_sq_bs = mode.allow_single_quote_backslash();
    let allow_dq_bs = mode.allow_double_quote_backslash();
    let allow_bracket = mode.allow_bracket_quotes();

    while i < len {
        let ch = sql[i..].chars().next().unwrap_or('\0');
        if is_sql_whitespace(DbEngine::Mysql, ch) {
            i += ch.len_utf8();
        } else if is_dash_comment_start(DbEngine::Mysql, sql, i) {
            i += 2;
            while i < len && bytes[i] != b'\n' {
                i += char_len_at(sql, i);
            }
        } else if bytes[i] == b'#' {
            i += 1;
            while i < len && bytes[i] != b'\n' {
                i += char_len_at(sql, i);
            }
        } else if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            while i < len {
                if i + 1 < len && bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    i += 2;
                    break;
                }
                i += char_len_at(sql, i);
            }
        } else if bytes[i] == b'\'' {
            pending_end_repeat = false;
            i = skip_quoted_bytes(sql, i, b'\'', allow_sq_bs);
        } else if bytes[i] == b'"' {
            pending_end_repeat = false;
            i = skip_quoted_bytes(sql, i, b'"', allow_dq_bs);
        } else if bytes[i] == b'`' {
            pending_end_repeat = false;
            i = skip_quoted_bytes(sql, i, b'`', false);
        } else if allow_bracket && bytes[i] == b'[' {
            pending_end_repeat = false;
            i = read_bracket_raw(sql, i, true).0;
        } else if bytes[i] == b'(' {
            pending_end_repeat = false;
            paren_depth += 1;
            i += 1;
        } else if bytes[i] == b')' {
            pending_end_repeat = false;
            paren_depth = paren_depth.saturating_sub(1);
            i += 1;
        } else if is_ident_start_for_engine(DbEngine::Mysql, ch) {
            let word_start = i;
            i += ch.len_utf8();
            while i < len {
                let nc = sql[i..].chars().next().unwrap_or('\0');
                if !is_ident_continue_for_engine(DbEngine::Mysql, nc) {
                    break;
                }
                i += nc.len_utf8();
            }
            if paren_depth != 0 {
                continue;
            }
            let word = &sql[word_start..i];
            if stop_after_end_repeat {
                if pending_end_repeat {
                    if word.eq_ignore_ascii_case("REPEAT") {
                        return Some(i);
                    }
                    pending_end_repeat = false;
                }
                if word.eq_ignore_ascii_case("CASE") {
                    expr_case_depth += 1;
                } else if word.eq_ignore_ascii_case("END") {
                    if expr_case_depth > 0 {
                        expr_case_depth -= 1;
                    } else {
                        pending_end_repeat = true;
                    }
                }
            } else if word.eq_ignore_ascii_case("CASE") {
                expr_case_depth += 1;
            } else if word.eq_ignore_ascii_case("END") && expr_case_depth > 0 {
                expr_case_depth -= 1;
            } else if expr_case_depth == 0 && word.eq_ignore_ascii_case(target_kw) {
                return Some(i);
            }
        } else {
            pending_end_repeat = false;
            i += ch.len_utf8();
        }
    }
    None
}

fn strip_mariadb_compound_segment_prefix(mut seg: &str, mode: MysqlQuoteMode) -> Option<&str> {
    let allow_dq_bs = mode.allow_double_quote_backslash();
    let allow_bracket = mode.allow_bracket_quotes();

    loop {
        let start = skip_mysql_ws_and_comments_idx(seg, 0);
        seg = trim_sql_whitespace_for_engine(DbEngine::Mysql, &seg[start..]);
        if seg.is_empty() {
            return None;
        }
        if let Some(after_set) = strip_all_mariadb_set_statement_prefixes(seg, mode) {
            seg = after_set;
            continue;
        }
        let bytes = seg.as_bytes();
        // Skip optional leading block label `<ident>:` (never `:=`).
        let label_end = if bytes[0] == b'`' {
            let (after, _, closed) = read_quoted_raw(seg, 0, b'`', false);
            closed.then_some(after)
        } else if mode.ansi_quotes && bytes[0] == b'"' {
            let (after, _, closed) = read_quoted_raw(seg, 0, b'"', allow_dq_bs);
            closed.then_some(after)
        } else if allow_bracket && bytes[0] == b'[' {
            let (after, _, closed) = read_bracket_raw(seg, 0, true);
            closed.then_some(after)
        } else if seg.chars().next().is_some_and(|c| is_ident_start_for_engine(DbEngine::Mysql, c))
        {
            let mut end = seg.chars().next().map_or(0, char::len_utf8);
            while end < seg.len() {
                let nc = seg[end..].chars().next().unwrap_or('\0');
                if !is_ident_continue_for_engine(DbEngine::Mysql, nc) {
                    break;
                }
                end += nc.len_utf8();
            }
            Some(end)
        } else {
            None
        };
        if let Some(id_end) = label_end {
            let after_ws = skip_mysql_ws_and_comments_idx(seg, id_end);
            if after_ws < bytes.len()
                && bytes[after_ws] == b':'
                && bytes.get(after_ws + 1) != Some(&b'=')
            {
                seg = &seg[after_ws + 1..];
                continue;
            }
        }

        let ch = seg.chars().next()?;
        if !is_ident_start_for_engine(DbEngine::Mysql, ch) {
            return Some(seg);
        }
        let mut end1 = ch.len_utf8();
        while end1 < seg.len() {
            let nc = seg[end1..].chars().next().unwrap_or('\0');
            if !is_ident_continue_for_engine(DbEngine::Mysql, nc) {
                break;
            }
            end1 += nc.len_utf8();
        }
        let w1 = &seg[..end1];

        if w1.eq_ignore_ascii_case("BEGIN") {
            let after_ws1 = skip_mysql_ws_and_comments_idx(seg, end1);
            if let Some(ch2) = seg[after_ws1..].chars().next()
                && is_ident_start_for_engine(DbEngine::Mysql, ch2)
            {
                let mut end2 = after_ws1 + ch2.len_utf8();
                while end2 < seg.len() {
                    let nc = seg[end2..].chars().next().unwrap_or('\0');
                    if !is_ident_continue_for_engine(DbEngine::Mysql, nc) {
                        break;
                    }
                    end2 += nc.len_utf8();
                }
                let w2 = &seg[after_ws1..end2];
                if w2.eq_ignore_ascii_case("WORK") {
                    return Some(seg);
                }
                if w2.eq_ignore_ascii_case("NOT") {
                    let after_ws2 = skip_mysql_ws_and_comments_idx(seg, end2);
                    if let Some(ch3) = seg[after_ws2..].chars().next()
                        && is_ident_start_for_engine(DbEngine::Mysql, ch3)
                    {
                        let mut end3 = after_ws2 + ch3.len_utf8();
                        while end3 < seg.len() {
                            let nc = seg[end3..].chars().next().unwrap_or('\0');
                            if !is_ident_continue_for_engine(DbEngine::Mysql, nc) {
                                break;
                            }
                            end3 += nc.len_utf8();
                        }
                        if seg[after_ws2..end3].eq_ignore_ascii_case("ATOMIC") {
                            seg = &seg[end3..];
                            continue;
                        }
                    }
                }
            }
            seg = &seg[end1..];
            continue;
        }

        if w1.eq_ignore_ascii_case("ELSE")
            || w1.eq_ignore_ascii_case("LOOP")
            || w1.eq_ignore_ascii_case("REPEAT")
        {
            seg = &seg[end1..];
            continue;
        }

        if w1.eq_ignore_ascii_case("IF")
            || w1.eq_ignore_ascii_case("ELSEIF")
            || w1.eq_ignore_ascii_case("CASE")
            || w1.eq_ignore_ascii_case("WHEN")
        {
            let Some(after_then) = find_mariadb_clause_kw_end(seg, end1, mode, "THEN", false) else {
                return Some(seg);
            };
            seg = &seg[after_then..];
            continue;
        }

        if w1.eq_ignore_ascii_case("WHILE") || w1.eq_ignore_ascii_case("FOR") {
            let Some(after_do) = find_mariadb_clause_kw_end(seg, end1, mode, "DO", false) else {
                return Some(seg);
            };
            seg = &seg[after_do..];
            continue;
        }

        if w1.eq_ignore_ascii_case("UNTIL") {
            if let Some(after_repeat) = find_mariadb_clause_kw_end(seg, end1, mode, "", true) {
                let tail = &seg[after_repeat..];
                let after_ws = skip_mysql_ws_and_comments_idx(tail, 0);
                let rem = trim_sql_whitespace_for_engine(DbEngine::Mysql, &tail[after_ws..]);
                if rem.is_empty() || is_single_ident_tail(rem, mode) {
                    return None;
                }
            }
            return Some(seg);
        }

        if w1.eq_ignore_ascii_case("END") {
            let mut pos = skip_mysql_ws_and_comments_idx(seg, end1);
            if let Some(ch2) = seg[pos..].chars().next()
                && is_ident_start_for_engine(DbEngine::Mysql, ch2)
            {
                let mut end2 = pos + ch2.len_utf8();
                while end2 < seg.len() {
                    let nc = seg[end2..].chars().next().unwrap_or('\0');
                    if !is_ident_continue_for_engine(DbEngine::Mysql, nc) {
                        break;
                    }
                    end2 += nc.len_utf8();
                }
                let w2 = &seg[pos..end2];
                if matches!(
                    w2.to_ascii_uppercase().as_str(),
                    "IF" | "CASE" | "LOOP" | "WHILE" | "REPEAT" | "FOR"
                ) {
                    pos = skip_mysql_ws_and_comments_idx(seg, end2);
                }
            }
            let rem = trim_sql_whitespace_for_engine(DbEngine::Mysql, &seg[pos..]);
            if rem.is_empty() || is_single_ident_tail(rem, mode) {
                return None;
            }
            return Some(seg);
        }

        if w1.eq_ignore_ascii_case("DECLARE")
            && let Some(after_handler) = find_declare_handler_body_start(seg, end1, mode)
        {
            seg = &seg[after_handler..];
            continue;
        }

        return Some(seg);
    }
}

fn read_mysql_word_at(sql: &str, pos: usize) -> Option<(&str, usize)> {
    let ch = sql[pos..].chars().next()?;
    if !is_ident_start_for_engine(DbEngine::Mysql, ch) {
        return None;
    }
    let len = sql.len();
    let mut end = pos + ch.len_utf8();
    while end < len {
        let nc = sql[end..].chars().next().unwrap_or('\0');
        if !is_ident_continue_for_engine(DbEngine::Mysql, nc) {
            break;
        }
        end += nc.len_utf8();
    }
    Some((&sql[pos..end], end))
}

fn is_single_ident_tail(rem: &str, mode: MysqlQuoteMode) -> bool {
    let bytes = rem.as_bytes();
    let end = if bytes[0] == b'`' {
        let (after, _, closed) = read_quoted_raw(rem, 0, b'`', false);
        if !closed {
            return false;
        }
        after
    } else if mode.ansi_quotes && bytes[0] == b'"' {
        let (after, _, closed) =
            read_quoted_raw(rem, 0, b'"', mode.allow_double_quote_backslash());
        if !closed {
            return false;
        }
        after
    } else if mode.allow_bracket_quotes() && bytes[0] == b'[' {
        let (after, _, closed) = read_bracket_raw(rem, 0, true);
        if !closed {
            return false;
        }
        after
    } else if rem.chars().next().is_some_and(|c| is_ident_start_for_engine(DbEngine::Mysql, c)) {
        let mut e = rem.chars().next().map_or(0, char::len_utf8);
        while e < rem.len() {
            let nc = rem[e..].chars().next().unwrap_or('\0');
            if !is_ident_continue_for_engine(DbEngine::Mysql, nc) {
                break;
            }
            e += nc.len_utf8();
        }
        e
    } else {
        return false;
    };
    let tail = skip_mysql_ws_and_comments_idx(rem, end);
    trim_sql_whitespace_for_engine(DbEngine::Mysql, &rem[tail..]).is_empty()
}

fn find_declare_handler_body_start(
    sql: &str,
    after_declare_pos: usize,
    mode: MysqlQuoteMode,
) -> Option<usize> {
    let bytes = sql.as_bytes();
    let len = bytes.len();
    let mut i = skip_mysql_ws_and_comments_idx(sql, after_declare_pos);
    let (action, next_action) = read_mysql_word_at(sql, i)?;
    if !action.eq_ignore_ascii_case("CONTINUE")
        && !action.eq_ignore_ascii_case("EXIT")
        && !action.eq_ignore_ascii_case("UNDO")
    {
        return None;
    }
    i = skip_mysql_ws_and_comments_idx(sql, next_action);
    let (kw_handler, next_handler) = read_mysql_word_at(sql, i)?;
    if !kw_handler.eq_ignore_ascii_case("HANDLER") {
        return None;
    }
    i = skip_mysql_ws_and_comments_idx(sql, next_handler);
    let (kw_for, next_for) = read_mysql_word_at(sql, i)?;
    if !kw_for.eq_ignore_ascii_case("FOR") {
        return None;
    }
    i = next_for;

    let allow_sq_bs = mode.allow_single_quote_backslash();
    let allow_dq_bs = mode.allow_double_quote_backslash();
    let allow_bracket = mode.allow_bracket_quotes();

    loop {
        i = skip_mysql_ws_and_comments_idx(sql, i);
        if i >= len {
            return None;
        }
        if bytes[i].is_ascii_digit() {
            while i < len && bytes[i].is_ascii_digit() {
                i += 1;
            }
        } else if bytes[i] == b'`' {
            let (after, _, closed) = read_quoted_raw(sql, i, b'`', false);
            if !closed {
                return None;
            }
            i = after;
        } else if mode.ansi_quotes && bytes[i] == b'"' {
            let (after, _, closed) = read_quoted_raw(sql, i, b'"', allow_dq_bs);
            if !closed {
                return None;
            }
            i = after;
        } else if allow_bracket && bytes[i] == b'[' {
            let (after, _, closed) = read_bracket_raw(sql, i, true);
            if !closed {
                return None;
            }
            i = after;
        } else if let Some((w, next_w)) = read_mysql_word_at(sql, i) {
            if w.eq_ignore_ascii_case("SQLSTATE") {
                let mut k = skip_mysql_ws_and_comments_idx(sql, next_w);
                if let Some((v, next_v)) = read_mysql_word_at(sql, k)
                    && v.eq_ignore_ascii_case("VALUE")
                {
                    k = skip_mysql_ws_and_comments_idx(sql, next_v);
                }
                if k >= len || (bytes[k] != b'\'' && !(!mode.ansi_quotes && bytes[k] == b'"')) {
                    return None;
                }
                let quote = bytes[k];
                let allow_bs = if quote == b'\'' { allow_sq_bs } else { allow_dq_bs };
                let (after, _, closed) = read_quoted_raw(sql, k, quote, allow_bs);
                if !closed {
                    return None;
                }
                i = after;
            } else if w.eq_ignore_ascii_case("NOT") {
                let k = skip_mysql_ws_and_comments_idx(sql, next_w);
                let (f, next_f) = read_mysql_word_at(sql, k)?;
                if !f.eq_ignore_ascii_case("FOUND") {
                    return None;
                }
                i = next_f;
            } else {
                i = next_w;
            }
        } else {
            return None;
        }

        i = skip_mysql_ws_and_comments_idx(sql, i);
        if bytes.get(i) == Some(&b',') {
            i += 1;
        } else {
            return (i < len).then_some(i);
        }
    }
}

pub(crate) fn extract_mariadb_compound_inner_statements(
    sql: &str,
    mode: MysqlQuoteMode,
) -> Option<Vec<String>> {
    if !starts_as_mariadb_compound_stmt(sql, mode) {
        return None;
    }
    let segments = split_mysql_semicolons_ignoring_compound(sql, mode);
    let mut inner = Vec::new();
    for seg in segments {
        if let Some(stripped) = strip_mariadb_compound_segment_prefix(&seg, mode)
            && !stripped.is_empty()
        {
            inner.push(stripped.to_owned());
        }
    }
    Some(inner)
}

fn trailing_backslash_count(bytes: &[u8]) -> usize {
    bytes.iter().rev().take_while(|&&b| b == b'\\').count()
}

fn pg_escape_string_closing_suffix(lit_body: &str) -> String {
    let bytes = lit_body.as_bytes();
    let len = bytes.len();
    if trailing_backslash_count(bytes) % 2 == 1 {
        return " '".to_owned();
    }

    let mut hex_start = len;
    while hex_start > 0 && bytes[hex_start - 1].is_ascii_hexdigit() {
        hex_start -= 1;
    }
    let hex_len = len - hex_start;

    if hex_start > 0 && trailing_backslash_count(&bytes[..hex_start - 1]) % 2 == 1 {
        match bytes[hex_start - 1] {
            b'u' if hex_len < 4 => return format!("{}1'", "0".repeat(4 - hex_len - 1)),
            b'U' if hex_len < 8 => return format!("{}1'", "0".repeat(8 - hex_len - 1)),
            b'x' if hex_len == 0 => return "41'".to_owned(),
            b'x' if hex_len == 1 && bytes[hex_start] == b'0' => return "1'".to_owned(),
            _ => {}
        }
    }

    if hex_len > 0
        && hex_len <= 2
        && bytes[hex_start..].iter().all(|&b| b == b'0')
        && trailing_backslash_count(&bytes[..hex_start]) % 2 == 1
    {
        return "1'".to_owned();
    }

    let mut run_bytes = Vec::new();
    let mut i = 0usize;
    while i < len {
        if bytes[i] == b'\\' {
            if let Some((byte, end)) = parse_pg_byte_escape(bytes, i) {
                run_bytes.push(byte);
                i = end;
            } else {
                run_bytes.clear();
                i += 1;
                if i < len {
                    i += char_len_at(lit_body, i);
                }
            }
        } else {
            run_bytes.clear();
            i += char_len_at(lit_body, i);
        }
    }
    if !run_bytes.is_empty()
        && !run_bytes.contains(&0)
        && let Err(err) = std::str::from_utf8(&run_bytes)
        && err.error_len().is_none()
    {
        let tail = &run_bytes[err.valid_up_to()..];
        let conts = match tail[0] {
            0xC2..=0xDF => "\\x80",
            0xE0 => {
                if tail.len() == 1 {
                    "\\xA0\\x80"
                } else {
                    "\\x80"
                }
            }
            0xE1..=0xEF => {
                if tail.len() == 1 {
                    "\\x80\\x80"
                } else {
                    "\\x80"
                }
            }
            0xF0 => match tail.len() {
                1 => "\\x90\\x80\\x80",
                2 => "\\x80\\x80",
                _ => "\\x80",
            },
            0xF1..=0xF4 => match tail.len() {
                1 => "\\x80\\x80\\x80",
                2 => "\\x80\\x80",
                _ => "\\x80",
            },
            _ => "",
        };
        if !conts.is_empty() {
            return format!("{conts}'");
        }
    }

    " '".to_owned()
}

fn pg_unicode_string_closing_suffix(lit_body: &str) -> String {
    let bytes = lit_body.as_bytes();
    let len = bytes.len();
    let mut hex_start = len;
    while hex_start > 0 && bytes[hex_start - 1].is_ascii_hexdigit() {
        hex_start -= 1;
    }
    let hex_len = len - hex_start;

    if hex_start > 0
        && bytes[hex_start - 1] == b'+'
        && trailing_backslash_count(&bytes[..hex_start - 1]) % 2 == 1
        && hex_len < 6
    {
        return format!("{}1'", "0".repeat(6 - hex_len - 1));
    }
    if trailing_backslash_count(&bytes[..hex_start]) % 2 == 1 && hex_len < 4 {
        return format!("{}1'", "0".repeat(4 - hex_len - 1));
    }

    " '".to_owned()
}

struct PgUnfinishedTail {
    start_byte: usize,
    closing_suffix: String,
}

struct SplitScanResult {
    statements: Vec<SqlStatement>,
    unclosed: bool,
    saw_escaped_quote: bool,
    dyn_offset: Option<usize>,
    next_pg_mode: Option<PostgresSessionMode>,
    pg_unfinished_tail: Option<PgUnfinishedTail>,
    tail_undelimited: bool,
}

fn split_statements_with_opts(
    input: &str,
    engine: DbEngine,
    mode: MysqlQuoteMode,
    pg_mode: &PostgresSessionMode,
    track_inline_sql_mode: bool,
    active_pg_known: bool,
) -> SplitScanResult {
    let bytes = input.as_bytes();
    let len = bytes.len();
    let mut out = Vec::new();
    let mut stmt_start = 0usize;
    let mut i = 0usize;
    let mut state = ScanState::Normal;
    let mut dollar_tag: Option<String> = None;
    let mut pg_quote_body_start = 0usize;
    let mut pg_plain_quote_is_unicode = false;
    let mut saw_escaped_quote = false;
    let mut active_mode = mode;
    let mut pg_scs = PgScsTracker::from_mode_with_known_active(pg_mode, active_pg_known);

    let allow_backtick = matches!(engine, DbEngine::Mysql | DbEngine::Sqlite);
    let mut allow_bracket_quote =
        engine == DbEngine::Sqlite || (engine == DbEngine::Mysql && active_mode.allow_bracket_quotes());
    let allow_hash_comment = engine == DbEngine::Mysql;
    let mut allow_single_quote_backslash = (engine == DbEngine::Mysql
        && active_mode.allow_single_quote_backslash())
        || (engine == DbEngine::Postgres && !pg_mode.standard_conforming_strings);
    let mut allow_double_quote_backslash =
        engine == DbEngine::Mysql && active_mode.allow_double_quote_backslash();
    let allow_dollar_quote = engine == DbEngine::Postgres;
    let allow_escape_string = engine == DbEngine::Postgres;
    let allow_nested_block_comments = engine == DbEngine::Postgres;
    let line_comment_ends_on_cr = engine == DbEngine::Postgres;

    let mut stmt_words: Vec<String> = Vec::new();
    let mut in_compound_body = false;
    let mut at_body_stmt_start = false;
    let mut prev_word_was_begin = false;
    let mut paren_depth = 0usize;
    let mut seen_pg_routine_sig = false;
    let mut escape_cont = EscapeContinuation::None;
    let mut mysql_exec_depth = 0usize;
    let mut mysql_comment_policy = active_mode
        .server_version
        .map_or(MysqlCommentPolicy::UnconditionalOnly, MysqlCommentPolicy::server);
    let mut mysql_compound = MysqlCompoundTracker::new();

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
                let ch = input[i..].chars().next().unwrap_or('\0');
                if bytes[i] == b'\'' {
                    if engine == DbEngine::Mysql {
                        mysql_compound.on_literal_or_non_colon_punct(None, paren_depth);
                    }
                    state = if allow_escape_string && escape_cont == EscapeContinuation::AfterNewline {
                        ScanState::EscapeSingleQuote
                    } else if allow_escape_string
                        && escape_cont == EscapeContinuation::PlainAfterNewline
                    {
                        ScanState::PlainSingleQuote
                    } else {
                        ScanState::SingleQuote
                    };
                    escape_cont = EscapeContinuation::None;
                    prev_word_was_begin = false;
                    at_body_stmt_start = false;
                    i += 1;
                    pg_quote_body_start = i;
                } else if allow_escape_string
                    && (bytes[i] == b'E' || bytes[i] == b'e')
                    && i + 1 < len
                    && bytes[i + 1] == b'\''
                    && !preceded_by_ident(engine, input, i)
                {
                    state = ScanState::EscapeSingleQuote;
                    escape_cont = EscapeContinuation::None;
                    prev_word_was_begin = false;
                    at_body_stmt_start = false;
                    i += 2;
                    pg_quote_body_start = i;
                } else if allow_escape_string
                    && (bytes[i] == b'U' || bytes[i] == b'u')
                    && i + 2 < len
                    && bytes[i + 1] == b'&'
                    && bytes[i + 2] == b'\''
                    && !preceded_by_ident(engine, input, i)
                {
                    state = ScanState::PlainSingleQuote;
                    pg_plain_quote_is_unicode = true;
                    escape_cont = EscapeContinuation::None;
                    prev_word_was_begin = false;
                    at_body_stmt_start = false;
                    i += 3;
                    pg_quote_body_start = i;
                } else if allow_escape_string
                    && matches!(bytes[i], b'X' | b'x' | b'B' | b'b')
                    && i + 1 < len
                    && bytes[i + 1] == b'\''
                    && !preceded_by_ident(engine, input, i)
                {
                    state = ScanState::PlainSingleQuote;
                    pg_plain_quote_is_unicode = false;
                    escape_cont = EscapeContinuation::None;
                    prev_word_was_begin = false;
                    at_body_stmt_start = false;
                    i += 2;
                    pg_quote_body_start = i;
                } else if bytes[i] == b'"' {
                    if engine == DbEngine::Mysql {
                        if active_mode.ansi_quotes {
                            mysql_compound.on_quoted_ident(paren_depth);
                        } else {
                            mysql_compound.on_literal_or_non_colon_punct(None, paren_depth);
                        }
                    }
                    state = ScanState::DoubleQuote;
                    escape_cont = EscapeContinuation::None;
                    prev_word_was_begin = false;
                    at_body_stmt_start = false;
                    i += 1;
                } else if allow_backtick && bytes[i] == b'`' {
                    if engine == DbEngine::Mysql {
                        mysql_compound.on_quoted_ident(paren_depth);
                    }
                    state = ScanState::BacktickQuote;
                    escape_cont = EscapeContinuation::None;
                    prev_word_was_begin = false;
                    at_body_stmt_start = false;
                    i += 1;
                } else if allow_bracket_quote && bytes[i] == b'[' {
                    if engine == DbEngine::Mysql {
                        mysql_compound.on_quoted_ident(paren_depth);
                    }
                    state = ScanState::BracketQuote;
                    escape_cont = EscapeContinuation::None;
                    prev_word_was_begin = false;
                    at_body_stmt_start = false;
                    i += 1;
                } else if is_dash_comment_start(engine, input, i) {
                    if engine == DbEngine::Mysql {
                        mysql_compound.on_whitespace();
                    }
                    state = ScanState::LineComment;
                    i += 2;
                } else if allow_hash_comment && bytes[i] == b'#' {
                    if engine == DbEngine::Mysql {
                        mysql_compound.on_whitespace();
                    }
                    state = ScanState::LineComment;
                    i += 1;
                } else if mysql_exec_depth > 0 && i + 1 < len && bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    escape_cont = EscapeContinuation::None;
                    mysql_exec_depth -= 1;
                    i += 2;
                } else if engine == DbEngine::Mysql
                    && let Some(header) =
                        parse_mysql_exec_comment_header(&input[i..], mysql_comment_policy.lexer())
                {
                    mysql_compound.on_whitespace();
                    escape_cont = EscapeContinuation::None;
                    if mysql_comment_policy.should_unwrap(header) {
                        i += mysql_comment_policy.unwrap_prefix_len(header);
                        mysql_exec_depth += 1;
                    } else {
                        i = skip_mysql_block_comment_body(
                            input,
                            i + header.prefix_len,
                            mysql_comment_policy.inactive_comment_allows_nested(header),
                        );
                    }
                } else if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
                    if engine == DbEngine::Mysql {
                        mysql_compound.on_whitespace();
                    }
                    escape_cont = EscapeContinuation::None;
                    state = ScanState::BlockComment(1);
                    i += 2;
                } else if allow_dollar_quote
                    && bytes[i] == b'$'
                    && !preceded_by_ident(engine, input, i)
                    && let Some(tag) = parse_dollar_tag(&input[i..])
                {
                    i += tag.len();
                    dollar_tag = Some(tag.to_owned());
                    escape_cont = EscapeContinuation::None;
                    prev_word_was_begin = false;
                    at_body_stmt_start = false;
                } else if bytes[i] == b';' {
                    escape_cont = EscapeContinuation::None;
                    let inside_mysql_compound =
                        engine == DbEngine::Mysql && mysql_compound.is_in_compound();
                    if in_compound_body || inside_mysql_compound || mysql_exec_depth > 0 {
                        i += 1;
                        prev_word_was_begin = false;
                        at_body_stmt_start = in_compound_body;
                        if inside_mysql_compound {
                            mysql_compound.on_semicolon();
                        }
                    } else if engine == DbEngine::Postgres && paren_depth > 0 && is_pg_rule_header(&stmt_words) {
                        i += 1;
                        prev_word_was_begin = false;
                    } else {
                        let prev_len = out.len();
                        push_trimmed_statement(input, stmt_start, i, engine, &mut out);
                        let emitted_stmt = out.len() > prev_len;
                        i += 1;
                        stmt_start = i;
                        stmt_words.clear();
                        in_compound_body = false;
                        at_body_stmt_start = false;
                        prev_word_was_begin = false;
                        paren_depth = 0;
                        seen_pg_routine_sig = false;
                        if engine == DbEngine::Mysql {
                            mysql_compound.reset();
                        }
                        if emitted_stmt
                            && track_inline_sql_mode
                            && engine == DbEngine::Mysql
                            && let Some(last_stmt) = out.last()
                        {
                            match parse_inline_mysql_set_sql_mode(&last_stmt.sql, active_mode, false) {
                                Some(Some(next_mode)) => {
                                    active_mode = next_mode;
                                    allow_single_quote_backslash =
                                        next_mode.allow_single_quote_backslash();
                                    allow_double_quote_backslash =
                                        next_mode.allow_double_quote_backslash();
                                    allow_bracket_quote = next_mode.allow_bracket_quotes();
                                    mysql_comment_policy = next_mode
                                        .server_version
                                        .map_or(MysqlCommentPolicy::UnconditionalOnly, MysqlCommentPolicy::server);
                                }
                                Some(None) => {
                                    return SplitScanResult {
                                        statements: out,
                                        unclosed: false,
                                        saw_escaped_quote,
                                        dyn_offset: Some(i),
                                        next_pg_mode: None,
                                        pg_unfinished_tail: None,
                                        tail_undelimited: false,
                                    };
                                }
                                None => {}
                            }
                        } else if emitted_stmt
                            && track_inline_sql_mode
                            && engine == DbEngine::Postgres
                            && let Some(last_stmt) = out.last()
                        {
                            let pause_for_uncertain = pg_scs.apply_with_fallback_scs(
                                &last_stmt.sql,
                                !allow_single_quote_backslash,
                            );
                            if let Some(scs) = pg_scs.active {
                                allow_single_quote_backslash = !scs;
                            } else if pause_for_uncertain {
                                return SplitScanResult {
                                    statements: out,
                                    unclosed: false,
                                    saw_escaped_quote,
                                    dyn_offset: Some(i),
                                    next_pg_mode: Some(pg_scs.to_session_mode()),
                                    pg_unfinished_tail: None,
                                    tail_undelimited: false,
                                };
                            }
                        }
                    }
                } else if is_ident_start_for_engine(engine, ch) {
                    escape_cont = EscapeContinuation::None;
                    let word_start = i;
                    i += ch.len_utf8();
                    while i < len {
                        let next_ch = input[i..].chars().next().unwrap_or('\0');
                        if !is_ident_continue_for_engine(engine, next_ch) {
                            break;
                        }
                        i += next_ch.len_utf8();
                    }
                    let word = &input[word_start..i];
                    if stmt_words.len() < 6 {
                        stmt_words.push(word.to_ascii_uppercase());
                    }
                    if engine == DbEngine::Mysql {
                        mysql_compound.on_word(word, paren_depth);
                    }
                    if in_compound_body {
                        if at_body_stmt_start && word.eq_ignore_ascii_case("END") {
                            in_compound_body = false;
                        }
                        at_body_stmt_start = false;
                        prev_word_was_begin = false;
                    } else if (engine == DbEngine::Sqlite
                        && paren_depth == 0
                        && word.eq_ignore_ascii_case("BEGIN")
                        && is_sqlite_trigger_header(&stmt_words))
                        || (engine == DbEngine::Postgres
                            && paren_depth == 0
                            && seen_pg_routine_sig
                            && prev_word_was_begin
                            && word.eq_ignore_ascii_case("ATOMIC")
                            && is_pg_routine_header(&stmt_words))
                    {
                        in_compound_body = true;
                        at_body_stmt_start = true;
                        prev_word_was_begin = false;
                    } else {
                        prev_word_was_begin = engine == DbEngine::Postgres
                            && paren_depth == 0
                            && seen_pg_routine_sig
                            && word.eq_ignore_ascii_case("BEGIN");
                    }
                } else if is_sql_whitespace(engine, ch) {
                    if ch == '\n' || ch == '\r' {
                        match escape_cont {
                            EscapeContinuation::AfterClose => {
                                escape_cont = EscapeContinuation::AfterNewline;
                            }
                            EscapeContinuation::PlainAfterClose => {
                                escape_cont = EscapeContinuation::PlainAfterNewline;
                            }
                            _ => {}
                        }
                    }
                    if engine == DbEngine::Mysql {
                        mysql_compound.on_whitespace();
                    }
                    i += ch.len_utf8();
                } else if engine == DbEngine::Mysql && ch == ':' && i + 1 < len && bytes[i + 1] == b'=' {
                    escape_cont = EscapeContinuation::None;
                    prev_word_was_begin = false;
                    at_body_stmt_start = false;
                    mysql_compound.on_literal_or_non_colon_punct(None, paren_depth);
                    i += 2;
                } else {
                    escape_cont = EscapeContinuation::None;
                    prev_word_was_begin = false;
                    at_body_stmt_start = false;
                    if ch == '(' {
                        if engine == DbEngine::Mysql {
                            mysql_compound.on_literal_or_non_colon_punct(Some('('), paren_depth);
                        }
                        paren_depth += 1;
                    } else if ch == ')' {
                        if engine == DbEngine::Postgres
                            && !in_compound_body
                            && paren_depth == 1
                            && is_pg_routine_header(&stmt_words)
                        {
                            seen_pg_routine_sig = true;
                        }
                        paren_depth = paren_depth.saturating_sub(1);
                        if engine == DbEngine::Mysql {
                            mysql_compound.on_literal_or_non_colon_punct(Some(')'), paren_depth);
                        }
                    } else if engine == DbEngine::Mysql {
                        mysql_compound.on_literal_or_non_colon_punct(Some(ch), paren_depth);
                    }
                    i += ch.len_utf8();
                }
            }
            ScanState::SingleQuote => {
                if bytes[i] == b'\\' {
                    if matches!(engine, DbEngine::Mysql | DbEngine::Postgres)
                        && i + 1 < len
                        && bytes[i + 1] == b'\''
                    {
                        saw_escaped_quote = true;
                    }
                    if allow_single_quote_backslash {
                        i += 1;
                        if i < len {
                            i += char_len_at(input, i);
                        }
                    } else {
                        i += 1;
                    }
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
            ScanState::PlainSingleQuote => {
                if bytes[i] == b'\'' {
                    if i + 1 < len && bytes[i + 1] == b'\'' {
                        i += 2;
                    } else {
                        state = ScanState::Normal;
                        escape_cont = EscapeContinuation::PlainAfterClose;
                        i += 1;
                    }
                } else {
                    i += char_len_at(input, i);
                }
            }
            ScanState::EscapeSingleQuote => {
                if bytes[i] == b'\\' {
                    i += 1;
                    if i < len {
                        i += char_len_at(input, i);
                    }
                } else if bytes[i] == b'\'' {
                    if i + 1 < len && bytes[i + 1] == b'\'' {
                        i += 2;
                    } else {
                        state = ScanState::Normal;
                        escape_cont = EscapeContinuation::AfterClose;
                        i += 1;
                    }
                } else {
                    i += char_len_at(input, i);
                }
            }
            ScanState::DoubleQuote => {
                if bytes[i] == b'\\' {
                    if engine == DbEngine::Mysql && i + 1 < len && bytes[i + 1] == b'"' {
                        saw_escaped_quote = true;
                    }
                    if allow_double_quote_backslash {
                        i += 1;
                        if i < len {
                            i += char_len_at(input, i);
                        }
                    } else {
                        i += 1;
                    }
                } else if bytes[i] == b'"' {
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
            ScanState::BracketQuote => {
                if bytes[i] == b']' {
                    if engine == DbEngine::Mysql && i + 1 < len && bytes[i + 1] == b']' {
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
                if bytes[i] == b'\n' || (line_comment_ends_on_cr && bytes[i] == b'\r') {
                    match escape_cont {
                        EscapeContinuation::AfterClose => {
                            escape_cont = EscapeContinuation::AfterNewline;
                        }
                        EscapeContinuation::PlainAfterClose => {
                            escape_cont = EscapeContinuation::PlainAfterNewline;
                        }
                        _ => {}
                    }
                    state = ScanState::Normal;
                }
                i += char_len_at(input, i);
            }
            ScanState::BlockComment(depth) => {
                if allow_nested_block_comments && i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
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

    let prev_len = out.len();
    if stmt_start < len {
        push_trimmed_statement(input, stmt_start, len, engine, &mut out);
    }
    let tail_emitted = out.len() > prev_len;

    let pg_unfinished_tail = if engine == DbEngine::Postgres {
        let closing_suffix = if let Some(tag) = dollar_tag {
            Some(format!(" {tag}"))
        } else {
            match state {
                ScanState::EscapeSingleQuote => {
                    Some(pg_escape_string_closing_suffix(&input[pg_quote_body_start..]))
                }
                ScanState::SingleQuote => {
                    if allow_single_quote_backslash {
                        Some(pg_escape_string_closing_suffix(&input[pg_quote_body_start..]))
                    } else {
                        Some(" '".to_owned())
                    }
                }
                ScanState::PlainSingleQuote => {
                    if pg_plain_quote_is_unicode {
                        Some(pg_unicode_string_closing_suffix(&input[pg_quote_body_start..]))
                    } else {
                        Some("0'".to_owned())
                    }
                }
                ScanState::DoubleQuote => Some("x\"".to_owned()),
                ScanState::BlockComment(depth) => {
                    Some(format!(" {}", "*/".repeat(depth.max(1))))
                }
                _ => None,
            }
        };
        closing_suffix.map(|closing_suffix| {
            let start_byte = if tail_emitted {
                out.last().map_or(stmt_start, |s| s.start_byte)
            } else {
                trim_span(input, stmt_start, len, engine).0
            };
            PgUnfinishedTail {
                start_byte,
                closing_suffix,
            }
        })
    } else {
        None
    };

    SplitScanResult {
        statements: out,
        unclosed: pg_unfinished_tail.is_some()
            || matches!(
                state,
                ScanState::SingleQuote | ScanState::PlainSingleQuote | ScanState::DoubleQuote
            )
            || (engine == DbEngine::Mysql && state == ScanState::BracketQuote),
        saw_escaped_quote,
        dyn_offset: None,
        next_pg_mode: None,
        pg_unfinished_tail,
        tail_undelimited: tail_emitted,
    }
}

fn scan_plain_top_level_statements(
    input: &str,
    engine: DbEngine,
    mode: MysqlQuoteMode,
    pg_standard_conforming_strings: bool,
) -> (Vec<String>, bool) {
    let bytes = input.as_bytes();
    let len = bytes.len();
    let mut parts = Vec::new();
    let mut stmt_start = 0usize;
    let mut i = 0usize;
    let mut state = ScanState::Normal;
    let mut dollar_tag: Option<String> = None;

    let allow_backtick = matches!(engine, DbEngine::Mysql | DbEngine::Sqlite);
    let allow_bracket =
        engine == DbEngine::Sqlite || (engine == DbEngine::Mysql && mode.allow_bracket_quotes());
    let allow_hash = engine == DbEngine::Mysql;
    let allow_sq_backslash = (engine == DbEngine::Mysql && mode.allow_single_quote_backslash())
        || (engine == DbEngine::Postgres && !pg_standard_conforming_strings);
    let allow_dq_backslash = engine == DbEngine::Mysql && mode.allow_double_quote_backslash();
    let allow_dollar = engine == DbEngine::Postgres;
    let allow_escape = engine == DbEngine::Postgres;
    let allow_nested_block_comments = engine == DbEngine::Postgres;
    let line_comment_ends_on_cr = engine == DbEngine::Postgres;

    let mut stmt_words: Vec<String> = Vec::new();
    let mut in_compound_body = false;
    let mut at_body_stmt_start = false;
    let mut prev_word_was_begin = false;
    let mut paren_depth = 0usize;
    let mut seen_pg_routine_sig = false;
    let mut escape_cont = EscapeContinuation::None;
    let mut mysql_compound = MysqlCompoundTracker::new();

    let mut cur_stmt_has_token = false;
    let mut saw_top_level_semi = false;
    let mut has_exec_after_semi = false;

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
                let ch = input[i..].chars().next().unwrap_or('\0');
                if bytes[i] == b'\'' {
                    cur_stmt_has_token = true;
                    if saw_top_level_semi {
                        has_exec_after_semi = true;
                    }
                    if engine == DbEngine::Mysql {
                        mysql_compound.on_literal_or_non_colon_punct(None, paren_depth);
                    }
                    state = if allow_escape && escape_cont == EscapeContinuation::AfterNewline {
                        ScanState::EscapeSingleQuote
                    } else if allow_escape && escape_cont == EscapeContinuation::PlainAfterNewline {
                        ScanState::PlainSingleQuote
                    } else {
                        ScanState::SingleQuote
                    };
                    escape_cont = EscapeContinuation::None;
                    prev_word_was_begin = false;
                    at_body_stmt_start = false;
                    i += 1;
                } else if allow_escape
                    && (bytes[i] == b'E' || bytes[i] == b'e')
                    && i + 1 < len
                    && bytes[i + 1] == b'\''
                    && !preceded_by_ident(engine, input, i)
                {
                    cur_stmt_has_token = true;
                    if saw_top_level_semi {
                        has_exec_after_semi = true;
                    }
                    state = ScanState::EscapeSingleQuote;
                    escape_cont = EscapeContinuation::None;
                    prev_word_was_begin = false;
                    at_body_stmt_start = false;
                    i += 2;
                } else if allow_escape
                    && (bytes[i] == b'U' || bytes[i] == b'u')
                    && i + 2 < len
                    && bytes[i + 1] == b'&'
                    && bytes[i + 2] == b'\''
                    && !preceded_by_ident(engine, input, i)
                {
                    cur_stmt_has_token = true;
                    if saw_top_level_semi {
                        has_exec_after_semi = true;
                    }
                    state = ScanState::PlainSingleQuote;
                    escape_cont = EscapeContinuation::None;
                    prev_word_was_begin = false;
                    at_body_stmt_start = false;
                    i += 3;
                } else if allow_escape
                    && matches!(bytes[i], b'X' | b'x' | b'B' | b'b')
                    && i + 1 < len
                    && bytes[i + 1] == b'\''
                    && !preceded_by_ident(engine, input, i)
                {
                    cur_stmt_has_token = true;
                    if saw_top_level_semi {
                        has_exec_after_semi = true;
                    }
                    state = ScanState::PlainSingleQuote;
                    escape_cont = EscapeContinuation::None;
                    prev_word_was_begin = false;
                    at_body_stmt_start = false;
                    i += 2;
                } else if bytes[i] == b'"' {
                    cur_stmt_has_token = true;
                    if saw_top_level_semi {
                        has_exec_after_semi = true;
                    }
                    if engine == DbEngine::Mysql {
                        if mode.ansi_quotes {
                            mysql_compound.on_quoted_ident(paren_depth);
                        } else {
                            mysql_compound.on_literal_or_non_colon_punct(None, paren_depth);
                        }
                    }
                    state = ScanState::DoubleQuote;
                    escape_cont = EscapeContinuation::None;
                    prev_word_was_begin = false;
                    at_body_stmt_start = false;
                    i += 1;
                } else if allow_backtick && bytes[i] == b'`' {
                    cur_stmt_has_token = true;
                    if saw_top_level_semi {
                        has_exec_after_semi = true;
                    }
                    if engine == DbEngine::Mysql {
                        mysql_compound.on_quoted_ident(paren_depth);
                    }
                    state = ScanState::BacktickQuote;
                    escape_cont = EscapeContinuation::None;
                    prev_word_was_begin = false;
                    at_body_stmt_start = false;
                    i += 1;
                } else if allow_bracket && bytes[i] == b'[' {
                    cur_stmt_has_token = true;
                    if saw_top_level_semi {
                        has_exec_after_semi = true;
                    }
                    if engine == DbEngine::Mysql {
                        mysql_compound.on_quoted_ident(paren_depth);
                    }
                    state = ScanState::BracketQuote;
                    escape_cont = EscapeContinuation::None;
                    prev_word_was_begin = false;
                    at_body_stmt_start = false;
                    i += 1;
                } else if is_dash_comment_start(engine, input, i) {
                    if engine == DbEngine::Mysql {
                        mysql_compound.on_whitespace();
                    }
                    state = ScanState::LineComment;
                    i += 2;
                } else if allow_hash && bytes[i] == b'#' {
                    if engine == DbEngine::Mysql {
                        mysql_compound.on_whitespace();
                    }
                    state = ScanState::LineComment;
                    i += 1;
                } else if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
                    if engine == DbEngine::Mysql {
                        mysql_compound.on_whitespace();
                    }
                    escape_cont = EscapeContinuation::None;
                    state = ScanState::BlockComment(1);
                    i += 2;
                } else if allow_dollar
                    && bytes[i] == b'$'
                    && !preceded_by_ident(engine, input, i)
                    && let Some(tag) = parse_dollar_tag(&input[i..])
                {
                    cur_stmt_has_token = true;
                    if saw_top_level_semi {
                        has_exec_after_semi = true;
                    }
                    i += tag.len();
                    dollar_tag = Some(tag.to_owned());
                    escape_cont = EscapeContinuation::None;
                    prev_word_was_begin = false;
                    at_body_stmt_start = false;
                } else if bytes[i] == b';' {
                    escape_cont = EscapeContinuation::None;
                    let inside_mysql_compound =
                        engine == DbEngine::Mysql && mysql_compound.is_in_compound();
                    if in_compound_body || inside_mysql_compound {
                        cur_stmt_has_token = true;
                        i += 1;
                        prev_word_was_begin = false;
                        at_body_stmt_start = in_compound_body;
                        if inside_mysql_compound {
                            mysql_compound.on_semicolon();
                        }
                    } else if engine == DbEngine::Postgres && paren_depth > 0 && is_pg_rule_header(&stmt_words) {
                        cur_stmt_has_token = true;
                        i += 1;
                        prev_word_was_begin = false;
                    } else {
                        if cur_stmt_has_token {
                            let (_, _, trimmed) = trim_span(input, stmt_start, i, engine);
                            if !trimmed.is_empty() {
                                parts.push(trimmed.to_owned());
                            }
                        }
                        saw_top_level_semi = true;
                        cur_stmt_has_token = false;
                        i += 1;
                        stmt_start = i;
                        stmt_words.clear();
                        in_compound_body = false;
                        at_body_stmt_start = false;
                        prev_word_was_begin = false;
                        paren_depth = 0;
                        seen_pg_routine_sig = false;
                        if engine == DbEngine::Mysql {
                            mysql_compound.reset();
                        }
                    }
                } else if is_ident_start_for_engine(engine, ch) {
                    cur_stmt_has_token = true;
                    if saw_top_level_semi {
                        has_exec_after_semi = true;
                    }
                    escape_cont = EscapeContinuation::None;
                    let word_start = i;
                    i += ch.len_utf8();
                    while i < len {
                        let next_ch = input[i..].chars().next().unwrap_or('\0');
                        if !is_ident_continue_for_engine(engine, next_ch) {
                            break;
                        }
                        i += next_ch.len_utf8();
                    }
                    let word = &input[word_start..i];
                    if stmt_words.len() < 6 {
                        stmt_words.push(word.to_ascii_uppercase());
                    }
                    if engine == DbEngine::Mysql {
                        mysql_compound.on_word(word, paren_depth);
                    }
                    if in_compound_body {
                        if at_body_stmt_start && word.eq_ignore_ascii_case("END") {
                            in_compound_body = false;
                        }
                        at_body_stmt_start = false;
                        prev_word_was_begin = false;
                    } else if (engine == DbEngine::Sqlite
                        && paren_depth == 0
                        && word.eq_ignore_ascii_case("BEGIN")
                        && is_sqlite_trigger_header(&stmt_words))
                        || (engine == DbEngine::Postgres
                            && paren_depth == 0
                            && seen_pg_routine_sig
                            && prev_word_was_begin
                            && word.eq_ignore_ascii_case("ATOMIC")
                            && is_pg_routine_header(&stmt_words))
                    {
                        in_compound_body = true;
                        at_body_stmt_start = true;
                        prev_word_was_begin = false;
                    } else {
                        prev_word_was_begin = engine == DbEngine::Postgres
                            && paren_depth == 0
                            && seen_pg_routine_sig
                            && word.eq_ignore_ascii_case("BEGIN");
                    }
                } else if is_sql_whitespace(engine, ch) {
                    if ch == '\n' || ch == '\r' {
                        match escape_cont {
                            EscapeContinuation::AfterClose => {
                                escape_cont = EscapeContinuation::AfterNewline;
                            }
                            EscapeContinuation::PlainAfterClose => {
                                escape_cont = EscapeContinuation::PlainAfterNewline;
                            }
                            _ => {}
                        }
                    }
                    if engine == DbEngine::Mysql {
                        mysql_compound.on_whitespace();
                    }
                    i += ch.len_utf8();
                } else if engine == DbEngine::Mysql && ch == ':' && i + 1 < len && bytes[i + 1] == b'=' {
                    cur_stmt_has_token = true;
                    if saw_top_level_semi {
                        has_exec_after_semi = true;
                    }
                    escape_cont = EscapeContinuation::None;
                    prev_word_was_begin = false;
                    at_body_stmt_start = false;
                    mysql_compound.on_literal_or_non_colon_punct(None, paren_depth);
                    i += 2;
                } else {
                    cur_stmt_has_token = true;
                    if saw_top_level_semi {
                        has_exec_after_semi = true;
                    }
                    escape_cont = EscapeContinuation::None;
                    prev_word_was_begin = false;
                    at_body_stmt_start = false;
                    if ch == '(' {
                        if engine == DbEngine::Mysql {
                            mysql_compound.on_literal_or_non_colon_punct(Some('('), paren_depth);
                        }
                        paren_depth += 1;
                    } else if ch == ')' {
                        if engine == DbEngine::Postgres
                            && !in_compound_body
                            && paren_depth == 1
                            && is_pg_routine_header(&stmt_words)
                        {
                            seen_pg_routine_sig = true;
                        }
                        paren_depth = paren_depth.saturating_sub(1);
                        if engine == DbEngine::Mysql {
                            mysql_compound.on_literal_or_non_colon_punct(Some(')'), paren_depth);
                        }
                    } else if engine == DbEngine::Mysql {
                        mysql_compound.on_literal_or_non_colon_punct(Some(ch), paren_depth);
                    }
                    i += ch.len_utf8();
                }
            }
            ScanState::SingleQuote => {
                if bytes[i] == b'\\' {
                    if allow_sq_backslash {
                        i += 1;
                        if i < len {
                            i += char_len_at(input, i);
                        }
                    } else {
                        i += 1;
                    }
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
            ScanState::PlainSingleQuote => {
                if bytes[i] == b'\'' {
                    if i + 1 < len && bytes[i + 1] == b'\'' {
                        i += 2;
                    } else {
                        state = ScanState::Normal;
                        escape_cont = EscapeContinuation::PlainAfterClose;
                        i += 1;
                    }
                } else {
                    i += char_len_at(input, i);
                }
            }
            ScanState::EscapeSingleQuote => {
                if bytes[i] == b'\\' {
                    i += 1;
                    if i < len {
                        i += char_len_at(input, i);
                    }
                } else if bytes[i] == b'\'' {
                    if i + 1 < len && bytes[i + 1] == b'\'' {
                        i += 2;
                    } else {
                        state = ScanState::Normal;
                        escape_cont = EscapeContinuation::AfterClose;
                        i += 1;
                    }
                } else {
                    i += char_len_at(input, i);
                }
            }
            ScanState::DoubleQuote => {
                if bytes[i] == b'\\' {
                    if allow_dq_backslash {
                        i += 1;
                        if i < len {
                            i += char_len_at(input, i);
                        }
                    } else {
                        i += 1;
                    }
                } else if bytes[i] == b'"' {
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
            ScanState::BracketQuote => {
                if bytes[i] == b']' {
                    if engine == DbEngine::Mysql && i + 1 < len && bytes[i + 1] == b']' {
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
                if bytes[i] == b'\n' || (line_comment_ends_on_cr && bytes[i] == b'\r') {
                    match escape_cont {
                        EscapeContinuation::AfterClose => {
                            escape_cont = EscapeContinuation::AfterNewline;
                        }
                        EscapeContinuation::PlainAfterClose => {
                            escape_cont = EscapeContinuation::PlainAfterNewline;
                        }
                        _ => {}
                    }
                    state = ScanState::Normal;
                }
                i += char_len_at(input, i);
            }
            ScanState::BlockComment(depth) => {
                if allow_nested_block_comments && i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
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

    if cur_stmt_has_token && stmt_start < len {
        let (_, _, trimmed) = trim_span(input, stmt_start, len, engine);
        if !trimmed.is_empty() {
            parts.push(trimmed.to_owned());
        }
    }

    (parts, has_exec_after_semi)
}

pub(crate) fn split_unwrapped_top_level_statements(
    engine: DbEngine,
    sql: &str,
    mode: MysqlQuoteMode,
) -> Vec<String> {
    scan_plain_top_level_statements(sql, engine, mode, true).0
}

pub(crate) fn unwrapped_sql_has_multiple_statements(
    engine: DbEngine,
    sql: &str,
    mode: MysqlQuoteMode,
) -> bool {
    scan_plain_top_level_statements(sql, engine, mode, true).1
}

/// Returns `true` if a single PostgreSQL statement slice contains multiple top-level
/// semicolon-separated statements under `standard_conforming_strings`.
pub fn postgres_slice_has_multiple_statements(
    sql: &str,
    standard_conforming_strings: bool,
) -> bool {
    let sql = sql.strip_prefix('\u{feff}').unwrap_or(sql);
    scan_plain_top_level_statements(
        sql,
        DbEngine::Postgres,
        MysqlQuoteMode::DEFAULT,
        standard_conforming_strings,
    )
    .1
}

/// Returns `true` if a single MySQL statement slice may unwrap into multiple server statements
/// separated by a top-level semicolon.
pub fn mysql_slice_has_multiple_statements(sql: &str, mysql_mode: Option<MysqlQuoteMode>) -> bool {
    mysql_slice_has_multiple_statements_with_server_version(sql, mysql_mode, None)
}

fn mysql_slice_has_multiple_statements_with_server_version(
    sql: &str,
    mysql_mode: Option<MysqlQuoteMode>,
    fallback_server_version: Option<MysqlServerVersion>,
) -> bool {
    let sql = sql.strip_prefix('\u{feff}').unwrap_or(sql);
    let check_mode = |mode: MysqlQuoteMode| {
        if let Some(sv) = mode.server_version {
            return mysql_policies_for_known_server(sql, sv)
                .into_iter()
                .any(|policy| {
                    let effective_mode = policy.apply_to_mode(mode);
                    let (unwrapped, _, _) =
                        unwrap_mysql_executable_comments_with_policy(sql, effective_mode, policy);
                    unwrapped_sql_has_multiple_statements(
                        DbEngine::Mysql,
                        &unwrapped,
                        effective_mode,
                    )
                });
        }
        let plausible = plausible_mysql_comment_policies(sql, mode, None);
        if plausible.budget_exhausted {
            return true;
        }
        plausible.policies.into_iter().any(|policy| {
            let effective_mode = policy.apply_to_mode(mode);
            let (unwrapped, _, _) =
                unwrap_mysql_executable_comments_with_policy(sql, effective_mode, policy);
            unwrapped_sql_has_multiple_statements(DbEngine::Mysql, &unwrapped, effective_mode)
        })
    };

    if let Some(mode) = mysql_mode {
        return check_mode(mode);
    }

    let mut modes = vec![
        MysqlQuoteMode::DEFAULT.with_server_version(fallback_server_version),
        MysqlQuoteMode::ANSI_QUOTES.with_server_version(fallback_server_version),
        MysqlQuoteMode::NO_BACKSLASH_ESCAPES.with_server_version(fallback_server_version),
        MysqlQuoteMode::ANSI_AND_NO_BACKSLASH.with_server_version(fallback_server_version),
    ];
    if sql.contains('[')
        && !matches!(
            fallback_server_version,
            Some(sv) if sv.flavor == MysqlFlavor::Mysql
        )
    {
        modes.push(MysqlQuoteMode::MSSQL.with_server_version(fallback_server_version));
        modes.push(MysqlQuoteMode::MSSQL_AND_NO_BACKSLASH.with_server_version(fallback_server_version));
    }
    modes.into_iter().any(check_mode)
}

/// Validates that each non-empty statement in a MySQL execution batch resolves to at most one
/// server statement under the live quote mode and inline `SET SESSION sql_mode` transitions.
pub fn validate_mysql_single_statement_batch(
    statements: &[String],
    initial_mode: Option<MysqlQuoteMode>,
) -> Result<(), String> {
    let known_server_version = initial_mode.and_then(|m| m.server_version);
    let mut active_mode = initial_mode;
    for raw_stmt in statements {
        let stmt = trim_sql_whitespace_for_engine(DbEngine::Mysql, raw_stmt);
        if stmt.is_empty() {
            continue;
        }
        if mysql_slice_has_multiple_statements_with_server_version(
            stmt,
            active_mode,
            known_server_version,
        ) {
            return Err(
                "MySQL statement expands to multiple semicolon-separated statements; split statements before executing"
                    .to_owned(),
            );
        }
        if let Some(cur) = active_mode {
            match parse_inline_mysql_set_sql_mode(stmt, cur, true) {
                Some(Some(next)) => active_mode = Some(next),
                Some(None) => active_mode = None,
                None => {}
            }
        }
    }
    Ok(())
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
    statement_at_cursor_for_engine_with_mysql_mode(input, anchor, focus, engine, None)
}

/// Returns the active selection if non-empty, or the statement containing the caret using an
/// optional live MySQL [`MysqlQuoteMode`].
pub fn statement_at_cursor_for_engine_with_mysql_mode(
    input: &str,
    anchor: usize,
    focus: usize,
    engine: DbEngine,
    mysql_mode: Option<MysqlQuoteMode>,
) -> Option<SqlStatement> {
    statement_at_cursor_for_engine_with_session_mode(
        input,
        anchor,
        focus,
        engine,
        SqlSessionMode::mysql(mysql_mode),
    )
}

/// Returns the active selection if non-empty, or the statement containing the caret using live
/// [`SqlSessionMode`] flags.
pub fn statement_at_cursor_for_engine_with_session_mode(
    input: &str,
    anchor: usize,
    focus: usize,
    engine: DbEngine,
    session_mode: SqlSessionMode,
) -> Option<SqlStatement> {
    let a = clamp_char_boundary(input, anchor);
    let b = clamp_char_boundary(input, focus);
    if a != b {
        let lo = a.min(b);
        let hi = a.max(b);
        let (start_byte, end_byte, trimmed) = trim_span(input, lo, hi, engine);
        if has_executable_sql_for_engine(trimmed, engine) {
            return Some(SqlStatement {
                sql: trimmed.to_owned(),
                start_byte,
                end_byte,
            });
        }
        return None;
    }

    let stmts = split_statements_for_engine_with_session_opts(input, engine, session_mode, false);
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
    let last_nl = input[..b].rfind('\n');
    if let Some(stmt) = stmts.iter().rfind(|stmt| stmt.end_byte < b)
        && last_nl.is_none_or(|nl| stmt.end_byte > nl)
    {
        return Some(stmt.clone());
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

fn is_dollar_tag_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || !b.is_ascii()
}

fn is_dollar_tag_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || !b.is_ascii()
}

fn parse_dollar_tag(slice: &str) -> Option<&str> {
    let rest = slice.strip_prefix('$')?;
    let end = rest.find('$')?;
    let inner = &rest[..end];
    if inner.is_empty()
        || (inner.bytes().next().is_some_and(is_dollar_tag_start)
            && inner.bytes().all(is_dollar_tag_continue))
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
    let (start_byte, end_byte, trimmed) = trim_span(input, raw_start, raw_end, engine);
    if has_executable_sql_for_engine(trimmed, engine) {
        out.push(SqlStatement {
            sql: trimmed.to_owned(),
            start_byte,
            end_byte,
        });
    }
}

fn trim_span(input: &str, mut start: usize, mut end: usize, engine: DbEngine) -> (usize, usize, &str) {
    while start < end {
        let ch = input[start..end].chars().next().unwrap_or('\0');
        if !is_sql_whitespace(engine, ch) {
            break;
        }
        start += ch.len_utf8();
    }
    while end > start {
        let ch = input[start..end].chars().next_back().unwrap_or('\0');
        if !is_sql_whitespace(engine, ch) {
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
    let allow_nested_block_comments = engine == DbEngine::Postgres;
    let line_comment_ends_on_cr = engine == DbEngine::Postgres;
    let mut mysql_exec_depth = 0usize;
    let mut i = 0usize;
    while i < len {
        let ch = sql[i..].chars().next().unwrap_or('\0');
        if is_sql_whitespace(engine, ch) {
            i += ch.len_utf8();
        } else if is_dash_comment_start(engine, sql, i) {
            i += 2;
            while i < len && bytes[i] != b'\n' && (!line_comment_ends_on_cr || bytes[i] != b'\r') {
                i += char_len_at(sql, i);
            }
        } else if allow_hash && bytes[i] == b'#' {
            i += 1;
            while i < len && bytes[i] != b'\n' {
                i += char_len_at(sql, i);
            }
        } else if mysql_exec_depth > 0 && i + 1 < len && bytes[i] == b'*' && bytes[i + 1] == b'/' {
            mysql_exec_depth -= 1;
            i += 2;
        } else if engine == DbEngine::Mysql
            && let Some(prefix_len) = mysql_executable_comment_prefix_len(&sql[i..])
        {
            i += prefix_len;
            mysql_exec_depth += 1;
        } else if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            let mut depth = 1usize;
            while i < len && depth > 0 {
                if allow_nested_block_comments && i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
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

#[derive(Clone, Debug, PartialEq, Eq)]
enum CredTok {
    Word(String),
    Literal(String),
    TypedLiteral(String),
    HexLiteral(String),
    DoubleQuoted(String),
    QuotedIdent(String),
    Number(String),
    Other,
    Punct(char),
}

fn is_cred_value_tok(engine: DbEngine, tok: &CredTok, allow_placeholder: bool) -> bool {
    matches!(tok, CredTok::Literal(_) | CredTok::TypedLiteral(_) | CredTok::HexLiteral(_))
        || (engine == DbEngine::Mysql && matches!(tok, CredTok::DoubleQuoted(_)))
        || (allow_placeholder && matches!(tok, CredTok::Punct('?')))
}

fn read_bracket_raw(input: &str, mut i: usize, allow_double_close_escape: bool) -> (usize, String, bool) {
    let bytes = input.as_bytes();
    let len = bytes.len();
    let mut out = String::new();
    let mut closed = false;
    i += 1;
    while i < len {
        if bytes[i] == b']' {
            if allow_double_close_escape && i + 1 < len && bytes[i + 1] == b']' {
                out.push(']');
                i += 2;
            } else {
                i += 1;
                closed = true;
                break;
            }
        } else {
            let ch = input[i..].chars().next().unwrap_or('\0');
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    (i, out, closed)
}

fn read_quoted_raw(input: &str, mut i: usize, quote: u8, allow_backslash: bool) -> (usize, String, bool) {
    let bytes = input.as_bytes();
    let len = bytes.len();
    let mut out = String::new();
    let mut closed = false;
    i += 1;
    while i < len {
        if allow_backslash && bytes[i] == b'\\' {
            i += 1;
            if i < len {
                let ch = input[i..].chars().next().unwrap_or('\0');
                match ch {
                    '0' => out.push('\0'),
                    'b' => out.push('\x08'),
                    'n' => out.push('\n'),
                    'r' => out.push('\r'),
                    't' => out.push('\t'),
                    'Z' => out.push('\x1a'),
                    '%' => out.push_str("\\%"),
                    '_' => out.push_str("\\_"),
                    _ => out.push(ch),
                }
                i += ch.len_utf8();
            }
        } else if bytes[i] == quote {
            if i + 1 < len && bytes[i + 1] == quote {
                out.push(quote as char);
                i += 2;
            } else {
                i += 1;
                closed = true;
                break;
            }
        } else {
            let ch = input[i..].chars().next().unwrap_or('\0');
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    (i, out, closed)
}

fn skip_quoted_bytes(input: &str, i: usize, quote: u8, allow_backslash: bool) -> usize {
    read_quoted_raw(input, i, quote, allow_backslash).0
}

fn skip_pg_ws_and_comments_with_newline(sql: &str, mut i: usize) -> (usize, bool) {
    let bytes = sql.as_bytes();
    let len = bytes.len();
    let mut saw_newline = false;
    while i < len {
        let ch = sql[i..].chars().next().unwrap_or('\0');
        if is_sql_whitespace(DbEngine::Postgres, ch) {
            if ch == '\n' || ch == '\r' {
                saw_newline = true;
            }
            i += ch.len_utf8();
        } else if is_dash_comment_start(DbEngine::Postgres, sql, i) {
            i += 2;
            while i < len && bytes[i] != b'\n' && bytes[i] != b'\r' {
                i += char_len_at(sql, i);
            }
            if i < len && (bytes[i] == b'\n' || bytes[i] == b'\r') {
                saw_newline = true;
                i += 1;
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
            break;
        }
    }
    (i, saw_newline)
}

fn push_unicode_scalar(out: &mut Vec<u8>, ch: char) {
    let mut buf = [0u8; 4];
    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
}

fn read_pg_escape_string_part(input: &str, mut i: usize) -> (usize, Vec<u8>, bool, bool) {
    let bytes = input.as_bytes();
    let len = bytes.len();
    let mut out = Vec::new();
    let mut closed = false;
    let mut used_raw_byte_escape = false;
    i += 1;
    while i < len {
        if bytes[i] == b'\'' {
            if i + 1 < len && bytes[i + 1] == b'\'' {
                out.push(b'\'');
                i += 2;
            } else {
                i += 1;
                closed = true;
                break;
            }
        } else if bytes[i] == b'\\' {
            i += 1;
            if i >= len {
                break;
            }
            match bytes[i] {
                b'b' => {
                    out.push(b'\x08');
                    i += 1;
                }
                b'f' => {
                    out.push(b'\x0c');
                    i += 1;
                }
                b'n' => {
                    out.push(b'\n');
                    i += 1;
                }
                b'r' => {
                    out.push(b'\r');
                    i += 1;
                }
                b't' => {
                    out.push(b'\t');
                    i += 1;
                }
                b'\\' | b'\'' => {
                    out.push(bytes[i]);
                    i += 1;
                }
                b'x' => {
                    let count = input[i + 1..]
                        .bytes()
                        .take(2)
                        .take_while(u8::is_ascii_hexdigit)
                        .count();
                    if count > 0
                        && let Ok(byte) = u8::from_str_radix(&input[i + 1..i + 1 + count], 16)
                    {
                        if byte == 0 {
                            return (i + 1 + count, Vec::new(), false, true);
                        }
                        used_raw_byte_escape = true;
                        out.push(byte);
                        i += 1 + count;
                    } else {
                        out.push(b'x');
                        i += 1;
                    }
                }
                b'u' => {
                    if input
                        .get(i + 1..i + 5)
                        .is_some_and(|hex| hex.bytes().all(|b| b.is_ascii_hexdigit()))
                        && let Ok(cp) = u32::from_str_radix(&input[i + 1..i + 5], 16)
                    {
                        if cp == 0 {
                            return (i + 5, Vec::new(), false, used_raw_byte_escape);
                        }
                        if (0xD800..=0xDBFF).contains(&cp)
                            && input.get(i + 5..i + 7) == Some("\\u")
                            && input
                                .get(i + 7..i + 11)
                                .is_some_and(|hex| hex.bytes().all(|b| b.is_ascii_hexdigit()))
                            && let Ok(low) = u32::from_str_radix(&input[i + 7..i + 11], 16)
                            && (0xDC00..=0xDFFF).contains(&low)
                            && let Some(ch) =
                                char::from_u32(0x10000 + (((cp - 0xD800) << 10) | (low - 0xDC00)))
                        {
                            push_unicode_scalar(&mut out, ch);
                            i += 11;
                            continue;
                        }
                        let Some(ch) = char::from_u32(cp) else {
                            return (i + 5, Vec::new(), false, used_raw_byte_escape);
                        };
                        push_unicode_scalar(&mut out, ch);
                        i += 5;
                    } else {
                        out.push(b'u');
                        i += 1;
                    }
                }
                b'U' => {
                    if input
                        .get(i + 1..i + 9)
                        .is_some_and(|hex| hex.bytes().all(|b| b.is_ascii_hexdigit()))
                        && let Ok(cp) = u32::from_str_radix(&input[i + 1..i + 9], 16)
                    {
                        if cp == 0 {
                            return (i + 9, Vec::new(), false, used_raw_byte_escape);
                        }
                        let Some(ch) = char::from_u32(cp) else {
                            return (i + 9, Vec::new(), false, used_raw_byte_escape);
                        };
                        push_unicode_scalar(&mut out, ch);
                        i += 9;
                    } else {
                        out.push(b'U');
                        i += 1;
                    }
                }
                b'0'..=b'7' => {
                    let count = input[i..]
                        .bytes()
                        .take(3)
                        .take_while(|b| (b'0'..=b'7').contains(b))
                        .count();
                    let val = u32::from_str_radix(&input[i..i + count], 8).unwrap_or(0);
                    // PG scan.l truncates 3-digit octal escapes to unsigned char (`\441` -> 0x21).
                    let byte = (val & 0xFF) as u8;
                    if byte == 0 {
                        return (i + count, Vec::new(), false, true);
                    }
                    used_raw_byte_escape = true;
                    out.push(byte);
                    i += count;
                }
                _ => {
                    let ch = input[i..].chars().next().unwrap_or('\0');
                    push_unicode_scalar(&mut out, ch);
                    i += ch.len_utf8();
                }
            }
        } else {
            let ch = input[i..].chars().next().unwrap_or('\0');
            push_unicode_scalar(&mut out, ch);
            i += ch.len_utf8();
        }
    }
    (i, out, closed, used_raw_byte_escape)
}

fn is_valid_pg_uescape_char(ch: char) -> bool {
    !ch.is_ascii_hexdigit()
        && !matches!(ch, '+' | '\'' | '"')
        && !is_sql_whitespace(DbEngine::Postgres, ch)
}

fn single_valid_uescape_char(s: &str) -> Option<char> {
    let mut chars = s.chars();
    let (Some(ch), None) = (chars.next(), chars.next()) else {
        return None;
    };
    is_valid_pg_uescape_char(ch).then_some(ch)
}

fn read_pg_uescape_char(sql: &str, i: usize) -> Option<(usize, char)> {
    let bytes = sql.as_bytes();
    if i >= bytes.len() {
        return None;
    }
    if bytes[i] == b'\'' {
        let (mut next_i, mut out, closed) = read_quoted_raw(sql, i, b'\'', false);
        if !closed {
            return None;
        }
        loop {
            let (j, saw_nl) = skip_pg_ws_and_comments_with_newline(sql, next_i);
            if saw_nl && j < bytes.len() && bytes[j] == b'\'' {
                let (k, part, part_closed) = read_quoted_raw(sql, j, b'\'', false);
                if !part_closed {
                    return None;
                }
                out.push_str(&part);
                next_i = k;
            } else {
                break;
            }
        }
        return single_valid_uescape_char(&out).map(|ch| (next_i, ch));
    }
    if (bytes[i] == b'E' || bytes[i] == b'e') && i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
        let (mut next_i, mut out, closed, mut used_raw_byte_escape) =
            read_pg_escape_string_part(sql, i + 1);
        if !closed {
            return None;
        }
        loop {
            let (j, saw_nl) = skip_pg_ws_and_comments_with_newline(sql, next_i);
            if saw_nl && j < bytes.len() && bytes[j] == b'\'' {
                let (k, part, part_closed, part_raw) = read_pg_escape_string_part(sql, j);
                if !part_closed {
                    return None;
                }
                used_raw_byte_escape |= part_raw;
                out.extend_from_slice(&part);
                next_i = k;
            } else {
                break;
            }
        }
        if out.len() == 1 {
            let ch = char::from(out[0]);
            return is_valid_pg_uescape_char(ch).then_some((next_i, ch));
        }
        if !used_raw_byte_escape {
            let decoded = String::from_utf8(out).ok()?;
            return single_valid_uescape_char(&decoded).map(|ch| (next_i, ch));
        }
        return None;
    }
    if bytes[i] == b'$'
        && let Some(tag) = parse_dollar_tag(&sql[i..])
    {
        let body_start = i + tag.len();
        let end_rel = sql[body_start..].find(tag)?;
        let body = &sql[body_start..body_start + end_rel];
        return single_valid_uescape_char(body).map(|ch| (body_start + end_rel + tag.len(), ch));
    }
    None
}

fn decode_pg_unicode_escapes(raw: &str, esc: char) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut i = 0usize;
    while i < raw.len() {
        let ch = raw[i..].chars().next().unwrap_or('\0');
        let next = i + ch.len_utf8();
        if ch == esc {
            let rest = &raw[next..];
            if rest.starts_with(esc) {
                out.push(esc);
                i = next + esc.len_utf8();
                continue;
            }
            if rest.starts_with('+')
                && rest.get(1..7).is_some_and(|hex| hex.bytes().all(|b| b.is_ascii_hexdigit()))
                && let Some(decoded) = u32::from_str_radix(&rest[1..7], 16).ok().and_then(char::from_u32)
            {
                out.push(decoded);
                i = next + 7;
                continue;
            }
            if rest.get(..4).is_some_and(|hex| hex.bytes().all(|b| b.is_ascii_hexdigit()))
                && let Some(decoded) = u32::from_str_radix(&rest[..4], 16).ok().and_then(char::from_u32)
            {
                out.push(decoded);
                i = next + 4;
                continue;
            }
        }
        out.push(ch);
        i = next;
    }
    out
}

fn read_optional_pg_uescape(sql: &str, after_lit: usize) -> (usize, char) {
    let (j, _) = skip_pg_ws_and_comments_with_newline(sql, after_lit);
    if sql.get(j..j + 7).is_some_and(|kw| kw.eq_ignore_ascii_case("UESCAPE"))
        && sql[j + 7..]
            .chars()
            .next()
            .is_none_or(|c| !is_ident_continue_for_engine(DbEngine::Postgres, c))
    {
        let (k, _) = skip_pg_ws_and_comments_with_newline(sql, j + 7);
        if let Some((after_esc, parsed_esc)) = read_pg_uescape_char(sql, k) {
            return (after_esc, parsed_esc);
        }
    }
    (after_lit, '\\')
}

fn read_pg_unicode_string(sql: &str, quote_pos: usize) -> (usize, String) {
    let bytes = sql.as_bytes();
    let (mut after_lit, mut raw, closed) = read_quoted_raw(sql, quote_pos, b'\'', false);
    if closed {
        loop {
            let (j, saw_nl) = skip_pg_ws_and_comments_with_newline(sql, after_lit);
            if saw_nl && j < bytes.len() && bytes[j] == b'\'' {
                let (k, part, part_closed) = read_quoted_raw(sql, j, b'\'', false);
                raw.push_str(&part);
                after_lit = k;
                if !part_closed {
                    break;
                }
            } else {
                break;
            }
        }
    }
    let (next_i, esc) = if closed {
        read_optional_pg_uescape(sql, after_lit)
    } else {
        (after_lit, '\\')
    };
    (next_i, decode_pg_unicode_escapes(&raw, esc))
}

fn read_pg_unicode_ident(sql: &str, quote_pos: usize) -> (usize, String) {
    let (after_quote, raw, closed) = read_quoted_raw(sql, quote_pos, b'"', false);
    let (next_i, esc) = if closed {
        read_optional_pg_uescape(sql, after_quote)
    } else {
        (after_quote, '\\')
    };
    (next_i, decode_pg_unicode_escapes(&raw, esc))
}

fn tokenize_statement_for_credentials_with_mode(
    engine: DbEngine,
    sql: &str,
    mysql_mode: MysqlQuoteMode,
    comment_policy: MysqlCommentPolicy,
    pg_standard_conforming_strings: bool,
) -> Vec<CredTok> {
    let sql = sql.strip_prefix('\u{feff}').unwrap_or(sql);
    let bytes = sql.as_bytes();
    let len = bytes.len();
    let mut toks = Vec::new();
    let mut i = 0usize;
    let mut escape_cont = EscapeContinuation::None;
    let mut mysql_exec_depth = 0usize;

    let effective_mysql_mode = comment_policy.apply_to_mode(mysql_mode);
    let allow_backtick = matches!(engine, DbEngine::Mysql | DbEngine::Sqlite);
    let allow_bracket = engine == DbEngine::Sqlite
        || (engine == DbEngine::Mysql && effective_mysql_mode.allow_bracket_quotes());
    let allow_hash = engine == DbEngine::Mysql;
    let allow_sq_backslash =
        engine == DbEngine::Mysql && effective_mysql_mode.allow_single_quote_backslash();
    let allow_dq_backslash =
        engine == DbEngine::Mysql && effective_mysql_mode.allow_double_quote_backslash();
    let allow_dollar = engine == DbEngine::Postgres;
    let allow_escape = engine == DbEngine::Postgres;
    let pg_legacy_escape = engine == DbEngine::Postgres && !pg_standard_conforming_strings;
    let allow_nested = engine == DbEngine::Postgres;
    let line_comment_ends_on_cr = engine == DbEngine::Postgres;

    while i < len {
        let ch = sql[i..].chars().next().unwrap_or('\0');
        if is_sql_whitespace(engine, ch) {
            if ch == '\n' || ch == '\r' {
                match escape_cont {
                    EscapeContinuation::AfterClose => {
                        escape_cont = EscapeContinuation::AfterNewline;
                    }
                    EscapeContinuation::PlainAfterClose => {
                        escape_cont = EscapeContinuation::PlainAfterNewline;
                    }
                    _ => {}
                }
            }
            i += ch.len_utf8();
        } else if is_dash_comment_start(engine, sql, i) {
            i += 2;
            while i < len && bytes[i] != b'\n' && (!line_comment_ends_on_cr || bytes[i] != b'\r') {
                i += char_len_at(sql, i);
            }
            if i < len && (bytes[i] == b'\n' || (line_comment_ends_on_cr && bytes[i] == b'\r')) {
                match escape_cont {
                    EscapeContinuation::AfterClose => {
                        escape_cont = EscapeContinuation::AfterNewline;
                    }
                    EscapeContinuation::PlainAfterClose => {
                        escape_cont = EscapeContinuation::PlainAfterNewline;
                    }
                    _ => {}
                }
                i += 1;
            }
        } else if allow_hash && bytes[i] == b'#' {
            i += 1;
            while i < len && bytes[i] != b'\n' {
                i += char_len_at(sql, i);
            }
            if i < len && bytes[i] == b'\n' {
                i += 1;
            }
        } else if mysql_exec_depth > 0 && i + 1 < len && bytes[i] == b'*' && bytes[i + 1] == b'/' {
            escape_cont = EscapeContinuation::None;
            mysql_exec_depth -= 1;
            i += 2;
        } else if engine == DbEngine::Mysql
            && let Some(header) = parse_mysql_exec_comment_header(&sql[i..], comment_policy.lexer())
        {
            escape_cont = EscapeContinuation::None;
            if comment_policy.should_unwrap(header) {
                i += comment_policy.unwrap_prefix_len(header);
                mysql_exec_depth += 1;
            } else {
                i = skip_mysql_block_comment_body(
                    sql,
                    i + header.prefix_len,
                    comment_policy.inactive_comment_allows_nested(header),
                );
            }
        } else if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            escape_cont = EscapeContinuation::None;
            i += 2;
            let mut depth = 1usize;
            while i < len && depth > 0 {
                if allow_nested && i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
                    depth += 1;
                    i += 2;
                } else if i + 1 < len && bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    depth -= 1;
                    i += 2;
                } else {
                    i += char_len_at(sql, i);
                }
            }
        } else if bytes[i] == b'\'' {
            let force_plain = allow_escape && escape_cont == EscapeContinuation::PlainAfterNewline;
            let use_escape = !force_plain
                && ((allow_escape && escape_cont == EscapeContinuation::AfterNewline)
                    || pg_legacy_escape);
            if force_plain {
                let (next_i, text, closed) = read_quoted_raw(sql, i, b'\'', false);
                i = next_i;
                escape_cont = if closed {
                    EscapeContinuation::PlainAfterClose
                } else {
                    EscapeContinuation::None
                };
                toks.push(CredTok::Literal(text));
            } else if use_escape {
                let (part_end, bytes_out, closed, _) = read_pg_escape_string_part(sql, i);
                i = if closed {
                    part_end
                } else {
                    skip_quoted_bytes(sql, i, b'\'', true)
                };
                escape_cont = if closed {
                    EscapeContinuation::AfterClose
                } else {
                    EscapeContinuation::None
                };
                toks.push(CredTok::Literal(
                    String::from_utf8_lossy(&bytes_out).into_owned(),
                ));
            } else {
                let (next_i, text, _) = read_quoted_raw(sql, i, b'\'', allow_sq_backslash);
                i = next_i;
                escape_cont = EscapeContinuation::None;
                toks.push(CredTok::Literal(text));
            }
        } else if allow_escape
            && (bytes[i] == b'E' || bytes[i] == b'e')
            && i + 1 < len
            && bytes[i + 1] == b'\''
            && !preceded_by_ident(engine, sql, i)
        {
            let (part_end, bytes_out, closed, _) = read_pg_escape_string_part(sql, i + 1);
            i = if closed {
                part_end
            } else {
                skip_quoted_bytes(sql, i + 1, b'\'', true)
            };
            escape_cont = if closed {
                EscapeContinuation::AfterClose
            } else {
                EscapeContinuation::None
            };
            toks.push(CredTok::Literal(
                String::from_utf8_lossy(&bytes_out).into_owned(),
            ));
        } else if allow_escape
            && (bytes[i] == b'U' || bytes[i] == b'u')
            && i + 2 < len
            && bytes[i + 1] == b'&'
            && bytes[i + 2] == b'\''
            && !preceded_by_ident(engine, sql, i)
        {
            escape_cont = EscapeContinuation::PlainAfterClose;
            let (next_i, text) = read_pg_unicode_string(sql, i + 2);
            i = next_i;
            toks.push(CredTok::Literal(text));
        } else if allow_escape
            && (bytes[i] == b'U' || bytes[i] == b'u')
            && i + 2 < len
            && bytes[i + 1] == b'&'
            && bytes[i + 2] == b'"'
            && !preceded_by_ident(engine, sql, i)
        {
            escape_cont = EscapeContinuation::None;
            let (next_i, ident) = read_pg_unicode_ident(sql, i + 2);
            i = next_i;
            toks.push(CredTok::DoubleQuoted(ident));
        } else if matches!(engine, DbEngine::Postgres | DbEngine::Mysql)
            && (bytes[i] == b'N' || bytes[i] == b'n')
            && i + 1 < len
            && bytes[i + 1] == b'\''
            && !preceded_by_ident(engine, sql, i)
        {
            if pg_legacy_escape {
                let (part_end, bytes_out, closed, _) = read_pg_escape_string_part(sql, i + 1);
                i = if closed {
                    part_end
                } else {
                    skip_quoted_bytes(sql, i + 1, b'\'', true)
                };
                escape_cont = if closed {
                    EscapeContinuation::AfterClose
                } else {
                    EscapeContinuation::None
                };
                let text = String::from_utf8_lossy(&bytes_out).into_owned();
                toks.push(if engine == DbEngine::Postgres {
                    CredTok::TypedLiteral(text)
                } else {
                    CredTok::Literal(text)
                });
            } else {
                escape_cont = EscapeContinuation::None;
                let (next_i, text, _) = read_quoted_raw(sql, i + 1, b'\'', allow_sq_backslash);
                i = next_i;
                toks.push(if engine == DbEngine::Postgres {
                    CredTok::TypedLiteral(text)
                } else {
                    CredTok::Literal(text)
                });
            }
        } else if allow_escape
            && (bytes[i] == b'B' || bytes[i] == b'b')
            && i + 1 < len
            && bytes[i + 1] == b'\''
            && !preceded_by_ident(engine, sql, i)
        {
            let (next_i, text, closed) = read_quoted_raw(sql, i + 1, b'\'', false);
            i = next_i;
            escape_cont = if closed {
                EscapeContinuation::PlainAfterClose
            } else {
                EscapeContinuation::None
            };
            toks.push(CredTok::TypedLiteral(text));
        } else if matches!(engine, DbEngine::Postgres | DbEngine::Mysql | DbEngine::Sqlite)
            && (bytes[i] == b'X' || bytes[i] == b'x')
            && i + 1 < len
            && bytes[i + 1] == b'\''
            && !preceded_by_ident(engine, sql, i)
        {
            escape_cont = EscapeContinuation::None;
            let (next_i, text, _) = read_quoted_raw(sql, i + 1, b'\'', false);
            i = next_i;
            toks.push(CredTok::HexLiteral(text.to_ascii_uppercase()));
        } else if engine == DbEngine::Mysql
            && bytes[i] == b'0'
            && i + 2 < len
            && bytes[i + 1] == b'x'
            && bytes[i + 2].is_ascii_hexdigit()
            && !preceded_by_ident(engine, sql, i)
        {
            escape_cont = EscapeContinuation::None;
            let hex_start = i + 2;
            let mut hex_end = hex_start;
            while hex_end < len && bytes[hex_end].is_ascii_hexdigit() {
                hex_end += 1;
            }
            i = hex_end;
            toks.push(CredTok::HexLiteral(sql[hex_start..hex_end].to_ascii_uppercase()));
        } else if bytes[i] == b'"' {
            escape_cont = EscapeContinuation::None;
            let (next_i, ident, _) = read_quoted_raw(sql, i, b'"', allow_dq_backslash);
            i = next_i;
            toks.push(CredTok::DoubleQuoted(ident));
        } else if allow_backtick && bytes[i] == b'`' {
            escape_cont = EscapeContinuation::None;
            let (next_i, ident, _) = read_quoted_raw(sql, i, b'`', false);
            i = next_i;
            toks.push(CredTok::QuotedIdent(ident.to_ascii_uppercase()));
        } else if allow_bracket && bytes[i] == b'[' {
            escape_cont = EscapeContinuation::None;
            let (next_i, ident, _) = read_bracket_raw(sql, i, engine == DbEngine::Mysql);
            i = next_i;
            toks.push(CredTok::QuotedIdent(ident.to_ascii_uppercase()));
        } else if allow_dollar
            && bytes[i] == b'$'
            && !preceded_by_ident(engine, sql, i)
            && let Some(tag) = parse_dollar_tag(&sql[i..])
        {
            escape_cont = EscapeContinuation::None;
            let body_start = i + tag.len();
            let mut body_end = len;
            i = body_start;
            while i < len {
                if bytes[i] == b'$' && sql[i..].starts_with(tag) {
                    body_end = i;
                    i += tag.len();
                    break;
                }
                i += char_len_at(sql, i);
            }
            toks.push(CredTok::Literal(sql[body_start..body_end].to_owned()));
        } else if is_ident_start_for_engine(engine, ch) {
            escape_cont = EscapeContinuation::None;
            let start = i;
            i += ch.len_utf8();
            while i < len {
                let next_ch = sql[i..].chars().next().unwrap_or('\0');
                if !is_ident_continue_for_engine(engine, next_ch) {
                    break;
                }
                i += next_ch.len_utf8();
            }
            toks.push(CredTok::Word(sql[start..i].to_ascii_uppercase()));
        } else if ch.is_ascii_digit() {
            escape_cont = EscapeContinuation::None;
            let start = i;
            i += 1;
            while i < len && bytes[i].is_ascii_digit() {
                i += 1;
            }
            toks.push(CredTok::Number(sql[start..i].to_owned()));
        } else if matches!(ch, '=' | '(' | ')' | '[' | ']' | ';' | ',' | '.' | '?' | '@' | ':' | '>' | '|') {
            escape_cont = EscapeContinuation::None;
            i += ch.len_utf8();
            toks.push(CredTok::Punct(ch));
        } else {
            escape_cont = EscapeContinuation::None;
            i += ch.len_utf8();
            toks.push(CredTok::Other);
        }
    }
    toks
}

fn is_cred_value_or_password_fn(
    engine: DbEngine,
    toks: &[CredTok],
    pos: usize,
    allow_placeholder: bool,
) -> bool {
    if toks
        .get(pos)
        .is_some_and(|t| is_cred_value_tok(engine, t, allow_placeholder))
    {
        return true;
    }
    if matches!(toks.get(pos), Some(CredTok::Word(w)) if w == "PASSWORD" || w == "OLD_PASSWORD") {
        if toks
            .get(pos + 1)
            .is_some_and(|t| is_cred_value_tok(engine, t, allow_placeholder))
        {
            return true;
        }
        if matches!(toks.get(pos + 1), Some(CredTok::Punct('(')))
            && toks
                .get(pos + 2)
                .is_some_and(|t| is_cred_value_tok(engine, t, allow_placeholder))
        {
            return true;
        }
    }
    false
}

fn is_sqlite_key_pragma_name(tok: &CredTok) -> bool {
    let (CredTok::Word(k) | CredTok::DoubleQuoted(k) | CredTok::QuotedIdent(k) | CredTok::Literal(k)) = tok else {
        return false;
    };
    matches!(
        k.to_ascii_uppercase().as_str(),
        "KEY" | "REKEY" | "HEXKEY" | "HEXREKEY" | "TEXTKEY" | "TEXTREKEY"
    )
}

fn contains_identified_secret(engine: DbEngine, toks: &[CredTok], allow_placeholder: bool) -> bool {
    let mut in_identified_with = false;
    for idx in 0..toks.len() {
        let CredTok::Word(w) = &toks[idx] else {
            continue;
        };
        if in_identified_with
            && (w == "BY" || w == "AS" || w == "USING")
            && is_cred_value_or_password_fn(engine, toks, idx + 1, allow_placeholder)
        {
            return true;
        }
        if w != "IDENTIFIED" {
            continue;
        }
        if matches!(
            toks.get(idx + 1),
            Some(CredTok::Word(op)) if op == "BY" || op == "AS" || op == "USING"
        ) && is_cred_value_or_password_fn(engine, toks, idx + 2, allow_placeholder)
        {
            return true;
        }
        if matches!(
            toks.get(idx + 1),
            Some(CredTok::Word(op)) if op == "WITH" || op == "VIA"
        ) {
            in_identified_with = true;
        }
    }
    false
}

fn contains_password_literal(engine: DbEngine, toks: &[CredTok], allow_placeholder: bool) -> bool {
    for idx in 0..toks.len() {
        let (CredTok::Word(w) | CredTok::DoubleQuoted(w)) = &toks[idx] else {
            continue;
        };
        if !w.eq_ignore_ascii_case("PASSWORD") {
            continue;
        }
        if toks
            .get(idx + 1)
            .is_some_and(|t| is_cred_value_tok(engine, t, allow_placeholder))
        {
            return true;
        }
        if matches!(toks.get(idx + 1), Some(CredTok::Punct('=')))
            && toks
                .get(idx + 2)
                .is_some_and(|t| is_cred_value_tok(engine, t, allow_placeholder))
        {
            return true;
        }
    }
    false
}

fn decode_percent_ascii_upper(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = String::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && bytes[i + 1].is_ascii_hexdigit()
            && bytes[i + 2].is_ascii_hexdigit()
            && let Ok(val) = u8::from_str_radix(&raw[i + 1..i + 3], 16)
        {
            out.push(char::from(val).to_ascii_uppercase());
            i += 3;
            continue;
        }
        out.push(char::from(bytes[i]).to_ascii_uppercase());
        i += 1;
    }
    out
}

fn sqlite_uri_has_key_param(uri: &str) -> bool {
    let before_frag = uri.split('#').next().unwrap_or(uri);
    let Some((_, query)) = before_frag.split_once('?') else {
        return false;
    };
    query.split('&').any(|param| {
        let key = param.split('=').next().unwrap_or("");
        let decoded = decode_percent_ascii_upper(key);
        matches!(
            decoded.as_str(),
            "KEY" | "HEXKEY" | "TEXTKEY" | "REKEY" | "HEXREKEY" | "TEXTREKEY"
        )
    })
}

fn sqlite_attach_sets_credentials(toks: &[CredTok]) -> bool {
    if !matches!(toks.first(), Some(CredTok::Word(w)) if w == "ATTACH") {
        return false;
    }
    let mut paren_depth = 0usize;
    let as_idx = (1..toks.len()).find(|&idx| match &toks[idx] {
        CredTok::Punct('(') => {
            paren_depth += 1;
            false
        }
        CredTok::Punct(')') => {
            paren_depth = paren_depth.saturating_sub(1);
            false
        }
        CredTok::Word(w) if paren_depth == 0 && w == "AS" => true,
        _ => false,
    });

    let target_toks = as_idx.map_or(&toks[1..], |idx| &toks[1..idx]);
    let mut joined_uri = String::new();
    for tok in target_toks {
        if let CredTok::Literal(s) | CredTok::DoubleQuoted(s) = tok {
            joined_uri.push_str(s);
        }
    }
    if sqlite_uri_has_key_param(&joined_uri) {
        return true;
    }

    if let Some(idx) = as_idx {
        let mut schema_paren_depth = 0usize;
        for (k, tok) in toks.iter().enumerate().skip(idx + 1) {
            match tok {
                CredTok::Punct('(') => schema_paren_depth += 1,
                CredTok::Punct(')') => schema_paren_depth = schema_paren_depth.saturating_sub(1),
                CredTok::Word(w)
                    if k >= idx + 2
                        && schema_paren_depth == 0
                        && w == "KEY"
                        && k + 1 < toks.len() =>
                {
                    return true;
                }
                _ => {}
            }
        }
        return false;
    }

    // Fallback for malformed/partial ATTACH missing `AS`: suppress if a trailing KEY expression follows.
    (2..toks.len().saturating_sub(1))
        .any(|idx| matches!(&toks[idx], CredTok::Word(w) if w == "KEY"))
}

fn strip_mariadb_set_statement_tokens(mut toks: &[CredTok]) -> &[CredTok] {
    while matches!(
        toks,
        [CredTok::Word(a), CredTok::Word(b), ..] if a == "SET" && b == "STATEMENT"
    ) {
        let mut depth = 0usize;
        let Some(for_idx) = toks[2..].iter().position(|tok| match tok {
            CredTok::Punct('(') => {
                depth += 1;
                false
            }
            CredTok::Punct(')') => {
                depth = depth.saturating_sub(1);
                false
            }
            CredTok::Word(w) if depth == 0 && w == "FOR" => true,
            _ => false,
        }) else {
            return toks;
        };
        toks = &toks[2 + for_idx + 1..];
    }
    toks
}

fn find_cred_top_level_kw(toks: &[CredTok], target_kw: &str) -> Option<usize> {
    let mut paren_depth = 0usize;
    let mut expr_case_depth = 0usize;
    for (idx, tok) in toks.iter().enumerate() {
        match tok {
            CredTok::Punct('(') => paren_depth += 1,
            CredTok::Punct(')') => paren_depth = paren_depth.saturating_sub(1),
            CredTok::Word(w) if paren_depth == 0 => {
                if w == "CASE" {
                    expr_case_depth += 1;
                } else if w == "END" && expr_case_depth > 0 {
                    expr_case_depth -= 1;
                } else if expr_case_depth == 0 && w == target_kw {
                    return Some(idx);
                }
            }
            _ => {}
        }
    }
    None
}

fn strip_mariadb_cred_prefixes(mut toks: &[CredTok]) -> &[CredTok] {
    loop {
        let next = strip_mariadb_set_statement_tokens(toks);
        if next.len() != toks.len() {
            toks = next;
            continue;
        }
        if let [
            CredTok::Word(_) | CredTok::QuotedIdent(_) | CredTok::DoubleQuoted(_),
            CredTok::Punct(':'),
            rest @ ..,
        ] = toks
            && matches!(
                rest.first(),
                Some(CredTok::Word(w))
                    if matches!(w.as_str(), "BEGIN" | "LOOP" | "WHILE" | "REPEAT" | "FOR")
            )
        {
            toks = rest;
            continue;
        }
        if let [
            CredTok::Word(a),
            CredTok::Word(b),
            CredTok::Word(c),
            rest @ ..,
        ] = toks
            && a == "BEGIN"
            && b == "NOT"
            && c == "ATOMIC"
        {
            toks = rest;
            continue;
        }
        if let [CredTok::Word(a), rest @ ..] = toks
            && a == "BEGIN"
            && !matches!(rest.first(), Some(CredTok::Word(w)) if w == "WORK")
        {
            toks = rest;
            continue;
        }
        if let [CredTok::Word(a), rest @ ..] = toks
            && matches!(a.as_str(), "ELSE" | "LOOP" | "REPEAT")
        {
            toks = rest;
            continue;
        }
        if let [CredTok::Word(a), rest @ ..] = toks
            && matches!(a.as_str(), "IF" | "ELSEIF" | "CASE" | "WHEN")
            && let Some(then_idx) = find_cred_top_level_kw(rest, "THEN")
        {
            toks = &rest[then_idx + 1..];
            continue;
        }
        if let [CredTok::Word(a), rest @ ..] = toks
            && (a == "WHILE" || a == "FOR")
            && let Some(do_idx) = find_cred_top_level_kw(rest, "DO")
        {
            toks = &rest[do_idx + 1..];
            continue;
        }
        if let [
            CredTok::Word(a),
            CredTok::Word(action),
            CredTok::Word(h),
            CredTok::Word(f),
            rest @ ..,
        ] = toks
            && a == "DECLARE"
            && matches!(action.as_str(), "CONTINUE" | "EXIT" | "UNDO")
            && h == "HANDLER"
            && f == "FOR"
        {
            let mut rest = rest;
            let matched = loop {
                let next = match rest {
                    [
                        CredTok::Word(s),
                        CredTok::Word(v),
                        CredTok::Literal(_) | CredTok::DoubleQuoted(_),
                        tail @ ..,
                    ] if s == "SQLSTATE" && v == "VALUE" => tail,
                    [
                        CredTok::Word(s),
                        CredTok::Literal(_) | CredTok::DoubleQuoted(_),
                        tail @ ..,
                    ] if s == "SQLSTATE" => tail,
                    [CredTok::Word(n), CredTok::Word(f2), tail @ ..]
                        if n == "NOT" && f2 == "FOUND" =>
                    {
                        tail
                    }
                    [
                        CredTok::Word(_) | CredTok::QuotedIdent(_) | CredTok::DoubleQuoted(_),
                        tail @ ..,
                    ] => tail,
                    [CredTok::Number(_) | CredTok::Other, ..] => {
                        let digits_len = rest
                            .iter()
                            .take_while(|t| matches!(t, CredTok::Number(_) | CredTok::Other))
                            .count();
                        &rest[digits_len..]
                    }
                    _ => break false,
                };
                rest = next;
                if let Some(tail) = rest.strip_prefix(&[CredTok::Punct(',')]) {
                    rest = tail;
                } else {
                    break true;
                }
            };
            if matched {
                toks = rest;
                continue;
            }
        }
        break;
    }
    toks
}

const MAX_CRED_EXPR_BYTES: usize = 16 * 1024;

struct CredTokIndex<'a> {
    toks: &'a [CredTok],
    matching_close: Vec<usize>,
}

impl<'a> CredTokIndex<'a> {
    fn new(toks: &'a [CredTok]) -> Self {
        let mut matching_close = vec![0usize; toks.len()];
        let mut stack: Vec<(usize, char)> = Vec::new();
        let mut open_parens = 0usize;
        let mut open_brackets = 0usize;
        for (idx, tok) in toks.iter().enumerate() {
            match tok {
                CredTok::Punct('(') => {
                    matching_close[idx] = usize::MAX;
                    stack.push((idx, ')'));
                    open_parens += 1;
                }
                CredTok::Punct('[') => {
                    matching_close[idx] = usize::MAX;
                    stack.push((idx, ']'));
                    open_brackets += 1;
                }
                CredTok::Punct(')') if open_parens > 0 => {
                    while let Some((open_idx, expected_close)) = stack.pop() {
                        if expected_close == ')' {
                            open_parens -= 1;
                            matching_close[open_idx] = idx;
                            break;
                        }
                        open_brackets -= 1;
                    }
                }
                CredTok::Punct(']') if open_brackets > 0 => {
                    while let Some((open_idx, expected_close)) = stack.pop() {
                        if expected_close == ']' {
                            open_brackets -= 1;
                            matching_close[open_idx] = idx;
                            break;
                        }
                        open_parens -= 1;
                    }
                }
                _ => {}
            }
        }
        Self {
            toks,
            matching_close,
        }
    }

    fn strip_wrapping_parens(&self, mut range: std::ops::Range<usize>) -> std::ops::Range<usize> {
        while range.start + 1 < range.end
            && matches!(self.toks[range.start], CredTok::Punct('('))
            && self.matching_close[range.start] == range.end - 1
        {
            range.start += 1;
            range.end -= 1;
        }
        range
    }

    fn is_single_call(&self, open_paren: usize, close_paren: usize) -> bool {
        open_paren < self.toks.len()
            && close_paren < self.toks.len()
            && matches!(self.toks[open_paren], CredTok::Punct('('))
            && self.matching_close[open_paren] == close_paren
    }

    fn skip_balanced(&self, idx: usize, end: usize) -> usize {
        if matches!(self.toks.get(idx), Some(CredTok::Punct('(' | '['))) {
            let close = self.matching_close[idx];
            if close < end {
                return close + 1;
            }
            return end;
        }
        idx + 1
    }

    fn split_top_level_commas(&self, range: std::ops::Range<usize>) -> Vec<std::ops::Range<usize>> {
        let mut parts = Vec::new();
        let mut part_start = range.start;
        let mut idx = range.start;
        while idx < range.end {
            match self.toks[idx] {
                CredTok::Punct('(' | '[') => idx = self.skip_balanced(idx, range.end),
                CredTok::Punct(',') => {
                    parts.push(part_start..idx);
                    idx += 1;
                    part_start = idx;
                }
                _ => idx += 1,
            }
        }
        parts.push(part_start..range.end);
        parts
    }

    fn find_top_level_kw(&self, range: std::ops::Range<usize>, target_kw: &str) -> Option<usize> {
        let mut case_depth = 0usize;
        let mut idx = range.start;
        while idx < range.end {
            match &self.toks[idx] {
                CredTok::Punct('(' | '[') => {
                    idx = self.skip_balanced(idx, range.end);
                    continue;
                }
                CredTok::Word(w) => {
                    if w == "CASE" {
                        case_depth += 1;
                    } else if w == "END" && case_depth > 0 {
                        case_depth -= 1;
                    } else if case_depth == 0 && w == target_kw {
                        return Some(idx);
                    }
                }
                _ => {}
            }
            idx += 1;
        }
        None
    }

    fn pg_split_last_top_level_type_cast(
        &self,
        range: std::ops::Range<usize>,
    ) -> Option<(std::ops::Range<usize>, std::ops::Range<usize>)> {
        let mut last_cast_idx = None;
        let mut idx = range.start;
        while idx < range.end {
            match self.toks[idx] {
                CredTok::Punct('(' | '[') => {
                    idx = self.skip_balanced(idx, range.end);
                    continue;
                }
                CredTok::Punct('|')
                    if idx + 1 < range.end
                        && matches!(self.toks[idx + 1], CredTok::Punct('|')) =>
                {
                    return None;
                }
                CredTok::Punct(':')
                    if idx + 1 < range.end
                        && matches!(self.toks[idx + 1], CredTok::Punct(':')) =>
                {
                    last_cast_idx = Some(idx);
                    idx += 2;
                    continue;
                }
                _ => {}
            }
            idx += 1;
        }
        let idx = last_cast_idx?;
        if idx == range.start {
            return None;
        }
        Some((range.start..idx, (idx + 2)..range.end))
    }
}

fn decode_hex_sql_bytes(hex: &str, budget: &mut CredEvalBudget) -> Option<Vec<u8>> {
    let hex = hex.trim();
    if hex.is_empty() || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let byte_len = hex.len().div_ceil(2);
    if byte_len > MAX_CRED_EXPR_BYTES {
        budget.mark_exhausted();
        return None;
    }
    budget.charge_bytes(byte_len)?;
    let mut bytes = Vec::with_capacity(byte_len);
    let mut idx = 0usize;
    if hex.len() % 2 == 1 {
        let b = u8::from_str_radix(&hex[..1], 16).ok()?;
        bytes.push(b);
        idx = 1;
    }
    while idx < hex.len() {
        let b = u8::from_str_radix(&hex[idx..idx + 2], 16).ok()?;
        bytes.push(b);
        idx += 2;
    }
    Some(bytes)
}

fn decode_hex_sql_string(hex: &str, budget: &mut CredEvalBudget) -> Option<String> {
    decode_hex_sql_bytes(hex, budget).map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

const MAX_CRED_EXPR_CANDIDATES: usize = 16;
const MAX_CRED_STMT_EVAL_STEPS: usize = 256;
const MAX_CRED_STMT_WORK_BYTES: usize = 128 * 1024;
const MAX_CRED_STMT_RECURSIVE_CALLS: usize = 32;
const MAX_CRED_TOTAL_EVAL_STEPS: usize = 2048;
const MAX_CRED_TOTAL_WORK_BYTES: usize = 1024 * 1024;
const MAX_CRED_TOTAL_RECURSIVE_CALLS: usize = 64;

struct CredEvalBudget {
    stmt_steps: usize,
    stmt_work_bytes: usize,
    stmt_recursive_calls: usize,
    stmt_exhausted: bool,
    total_steps: usize,
    total_work_bytes: usize,
    total_recursive_calls: usize,
    total_exhausted: bool,
}

impl CredEvalBudget {
    const fn new() -> Self {
        Self {
            stmt_steps: 0,
            stmt_work_bytes: 0,
            stmt_recursive_calls: 0,
            stmt_exhausted: false,
            total_steps: 0,
            total_work_bytes: 0,
            total_recursive_calls: 0,
            total_exhausted: false,
        }
    }

    const fn begin_stmt(&mut self) {
        self.stmt_steps = 0;
        self.stmt_work_bytes = 0;
        self.stmt_recursive_calls = 0;
        self.stmt_exhausted = false;
    }

    const fn mark_exhausted(&mut self) {
        self.stmt_exhausted = true;
    }

    const fn is_exhausted(&self) -> bool {
        self.stmt_exhausted || self.total_exhausted
    }

    fn charge_step(&mut self) -> Option<()> {
        self.stmt_steps = self.stmt_steps.saturating_add(1);
        self.total_steps = self.total_steps.saturating_add(1);
        if self.total_steps > MAX_CRED_TOTAL_EVAL_STEPS {
            self.total_exhausted = true;
            return None;
        }
        if self.stmt_steps > MAX_CRED_STMT_EVAL_STEPS {
            self.stmt_exhausted = true;
            return None;
        }
        Some(())
    }

    fn charge_bytes(&mut self, bytes: usize) -> Option<()> {
        self.stmt_work_bytes = self.stmt_work_bytes.saturating_add(bytes);
        self.total_work_bytes = self.total_work_bytes.saturating_add(bytes);
        if self.total_work_bytes > MAX_CRED_TOTAL_WORK_BYTES {
            self.total_exhausted = true;
            return None;
        }
        if self.stmt_work_bytes > MAX_CRED_STMT_WORK_BYTES {
            self.stmt_exhausted = true;
            return None;
        }
        Some(())
    }

    fn charge_recursive_call(&mut self) -> Option<()> {
        self.stmt_recursive_calls = self.stmt_recursive_calls.saturating_add(1);
        self.total_recursive_calls = self.total_recursive_calls.saturating_add(1);
        if self.total_recursive_calls > MAX_CRED_TOTAL_RECURSIVE_CALLS {
            self.total_exhausted = true;
            return None;
        }
        if self.stmt_recursive_calls > MAX_CRED_STMT_RECURSIVE_CALLS {
            self.stmt_exhausted = true;
            return None;
        }
        Some(())
    }
}

fn contains_ascii_ignore_case(haystack: &str, needle_upper: &[u8]) -> bool {
    let bytes = haystack.as_bytes();
    bytes
        .windows(needle_upper.len())
        .any(|win| win.eq_ignore_ascii_case(needle_upper))
}

fn mysql_inner_sql_may_set_credentials(sql: &str) -> bool {
    const NEEDLES: [&[u8]; 10] = [
        b"PASSWORD",
        b"PASSWD",
        b"SECRET",
        b"IDENTIFIED",
        b"REPLACE",
        b"PREPARE",
        b"EXECUTE",
        b"SET",
        b"/*!",
        b"/*M!",
    ];
    NEEDLES
        .iter()
        .any(|needle| contains_ascii_ignore_case(sql, needle))
}

fn mysql_expr_may_produce_sql(toks: &[CredTok], ansi_quotes: bool) -> bool {
    toks.iter().any(|tok| match tok {
        CredTok::Literal(_) | CredTok::HexLiteral(_) => true,
        CredTok::DoubleQuoted(_) => !ansi_quotes,
        CredTok::Word(w) => w == "CHAR",
        _ => false,
    })
}

fn mysql_expr_has_string_literal(toks: &[CredTok], ansi_quotes: bool) -> bool {
    toks.iter().any(|t| {
        matches!(t, CredTok::Literal(_) | CredTok::HexLiteral(_))
            || (!ansi_quotes && matches!(t, CredTok::DoubleQuoted(_)))
    })
}

fn push_unique_cred_candidate(
    out: &mut Vec<String>,
    cand: String,
    budget: &mut CredEvalBudget,
) -> Option<()> {
    if cand.len() > MAX_CRED_EXPR_BYTES {
        budget.mark_exhausted();
        return None;
    }
    budget.charge_bytes(cand.len())?;
    if !out.contains(&cand) {
        if out.len() >= MAX_CRED_EXPR_CANDIDATES {
            budget.mark_exhausted();
            return None;
        }
        out.push(cand);
    }
    Some(())
}

fn bounded_replace(
    haystack: &str,
    from: &str,
    to: &str,
    max_bytes: usize,
    budget: &mut CredEvalBudget,
) -> Option<String> {
    budget.charge_bytes(haystack.len())?;
    if haystack.len() > max_bytes {
        budget.mark_exhausted();
        return None;
    }
    if from.is_empty() {
        budget.charge_bytes(haystack.len())?;
        return Some(haystack.to_owned());
    }
    let mut out = String::new();
    let mut last = 0usize;
    for (idx, _) in haystack.match_indices(from) {
        budget.charge_step()?;
        let chunk = &haystack[last..idx];
        let Some(next_len) = out.len().checked_add(chunk.len()).and_then(|n| n.checked_add(to.len())) else {
            budget.mark_exhausted();
            return None;
        };
        if next_len > max_bytes {
            budget.mark_exhausted();
            return None;
        }
        out.push_str(chunk);
        out.push_str(to);
        last = idx + from.len();
    }
    let tail = &haystack[last..];
    let Some(final_len) = out.len().checked_add(tail.len()) else {
        budget.mark_exhausted();
        return None;
    };
    if final_len > max_bytes {
        budget.mark_exhausted();
        return None;
    }
    out.push_str(tail);
    budget.charge_bytes(out.len())?;
    Some(out)
}

fn combine_cred_expr_parts(
    lists: Vec<Vec<String>>,
    sep: &str,
    budget: &mut CredEvalBudget,
) -> Option<Vec<String>> {
    let mut acc = vec![String::new()];
    for (pos, vals) in lists.into_iter().enumerate() {
        if vals.is_empty() {
            return None;
        }
        let sep_len = if pos > 0 { sep.len() } else { 0 };
        let mut next = Vec::new();
        for prefix in &acc {
            for val in &vals {
                budget.charge_step()?;
                let Some(total) = prefix
                    .len()
                    .checked_add(sep_len)
                    .and_then(|n| n.checked_add(val.len()))
                else {
                    budget.mark_exhausted();
                    return None;
                };
                if total > MAX_CRED_EXPR_BYTES {
                    budget.mark_exhausted();
                    return None;
                }
                let mut combined = String::with_capacity(total);
                combined.push_str(prefix);
                if pos > 0 {
                    combined.push_str(sep);
                }
                combined.push_str(val);
                push_unique_cred_candidate(&mut next, combined, budget)?;
            }
        }
        acc = next;
    }
    Some(acc)
}

#[derive(Clone, Copy)]
struct MysqlCredEvalOpts {
    ansi_quotes: bool,
    allow_unresolved_non_literal_branch: bool,
    strict_concat: bool,
}

fn extract_cred_case_branches(
    index: &CredTokIndex<'_>,
    range: std::ops::Range<usize>,
) -> Option<Vec<std::ops::Range<usize>>> {
    let mut case_depth = 0isize;
    let mut branch_start: Option<usize> = None;
    let mut branches = Vec::new();
    let mut idx = range.start;
    while idx < range.end {
        match &index.toks[idx] {
            CredTok::Punct('(' | '[') => {
                let close = index.matching_close[idx];
                if close >= range.end {
                    return None;
                }
                idx = close + 1;
                continue;
            }
            CredTok::Punct(')' | ']') => return None,
            CredTok::Word(w) => {
                if w == "CASE" {
                    case_depth += 1;
                } else if w == "END" {
                    case_depth -= 1;
                    if case_depth < 0 {
                        return None;
                    }
                } else if case_depth == 0 {
                    if w == "WHEN" || w == "ELSE" {
                        if let Some(start) = branch_start.take() {
                            branches.push(start..idx);
                        }
                        if w == "ELSE" {
                            branch_start = Some(idx + 1);
                        }
                    } else if w == "THEN" && branch_start.is_none() {
                        branch_start = Some(idx + 1);
                    }
                }
            }
            _ => {}
        }
        idx += 1;
    }
    if case_depth != 0 {
        return None;
    }
    if let Some(start) = branch_start {
        branches.push(start..range.end);
    }
    (!branches.is_empty()).then_some(branches)
}

fn eval_cred_branches(
    index: &CredTokIndex<'_>,
    branches: &[std::ops::Range<usize>],
    opts: MysqlCredEvalOpts,
    depth: usize,
    budget: &mut CredEvalBudget,
) -> Option<Vec<String>> {
    let mut out = Vec::new();
    for branch in branches {
        budget.charge_step()?;
        if matches!(&index.toks[branch.clone()], [CredTok::Word(w)] if w == "NULL") {
            continue;
        }
        match mysql_eval_cred_sql_expr_candidates(index, branch.clone(), opts, depth + 1, budget) {
            Some(vals) => {
                for val in vals {
                    push_unique_cred_candidate(&mut out, val, budget)?;
                }
            }
            None => {
                if !budget.is_exhausted()
                    && opts.allow_unresolved_non_literal_branch
                    && !mysql_expr_has_string_literal(&index.toks[branch.clone()], opts.ansi_quotes)
                {
                    continue;
                }
                return None;
            }
        }
    }
    (!out.is_empty()).then_some(out)
}

fn eval_mysql_concat_args(
    index: &CredTokIndex<'_>,
    args: &[std::ops::Range<usize>],
    seps: Option<Vec<String>>,
    opts: MysqlCredEvalOpts,
    depth: usize,
    budget: &mut CredEvalBudget,
) -> Option<Vec<String>> {
    if args.is_empty() {
        return None;
    }
    if opts.strict_concat {
        let strict_arg_opts = MysqlCredEvalOpts {
            allow_unresolved_non_literal_branch: false,
            ..opts
        };
        let mut lists = Vec::with_capacity(args.len());
        for arg in args {
            lists.push(mysql_eval_cred_sql_expr_candidates(
                index,
                arg.clone(),
                strict_arg_opts,
                depth + 1,
                budget,
            )?);
        }
        return match seps {
            None => combine_cred_expr_parts(lists, "", budget),
            Some(sep_list) => {
                let mut out = Vec::new();
                for sep in &sep_list {
                    for cand in combine_cred_expr_parts(lists.clone(), sep, budget)? {
                        push_unique_cred_candidate(&mut out, cand, budget)?;
                    }
                }
                (!out.is_empty()).then_some(out)
            }
        };
    }

    let mut any_const = false;
    let mut unresolved_count = 0usize;
    let mut resolved = Vec::with_capacity(args.len());
    for arg in args {
        if let Some(vals) =
            mysql_eval_cred_sql_expr_candidates(index, arg.clone(), opts, depth + 1, budget)
        {
            any_const = true;
            resolved.push(Some(vals));
        } else if budget.is_exhausted()
            || mysql_expr_has_string_literal(&index.toks[arg.clone()], opts.ansi_quotes)
        {
            return None;
        } else {
            unresolved_count += 1;
            resolved.push(None);
        }
    }
    if !any_const {
        return None;
    }

    let sep_vals = seps.unwrap_or_else(|| vec![String::new()]);
    let combine_resolved =
        |budget: &mut CredEvalBudget, slot_fn: &dyn Fn() -> Vec<String>| -> Option<Vec<String>> {
            let lists: Vec<Vec<String>> = resolved
                .iter()
                .map(|entry| entry.as_ref().cloned().unwrap_or_else(slot_fn))
                .collect();
            let mut out = Vec::new();
            for sep in &sep_vals {
                for cand in combine_cred_expr_parts(lists.clone(), sep, budget)? {
                    push_unique_cred_candidate(&mut out, cand, budget)?;
                }
            }
            (!out.is_empty()).then_some(out)
        };

    if unresolved_count == 0 {
        return combine_resolved(budget, &|| vec![String::new()]);
    }
    let per_slot_combos = resolved
        .iter()
        .try_fold(sep_vals.len(), |acc, entry| {
            acc.checked_mul(entry.as_ref().map_or(2, Vec::len))
        });
    if unresolved_count <= 3
        && per_slot_combos.is_some_and(|n| n <= MAX_CRED_EXPR_CANDIDATES)
        && let Some(cands) =
            combine_resolved(budget, &|| vec![" __TV_EXPR__ ".to_owned(), "'".to_owned()])
    {
        return Some(cands);
    }

    let mut out = Vec::new();
    for placeholder in [" __TV_EXPR__ ", "'"] {
        if let Some(cands) = combine_resolved(budget, &|| vec![placeholder.to_owned()]) {
            for cand in cands {
                push_unique_cred_candidate(&mut out, cand, budget)?;
            }
        } else if budget.is_exhausted() {
            return None;
        }
    }
    (!out.is_empty()).then_some(out)
}

fn eval_mysql_char_candidates(
    index: &CredTokIndex<'_>,
    inner: std::ops::Range<usize>,
    budget: &mut CredEvalBudget,
) -> Option<Vec<String>> {
    budget.charge_step()?;
    if inner.is_empty() {
        return None;
    }
    let mut args = index.split_top_level_commas(inner);
    if let Some(last) = args.last_mut()
        && let Some(using_idx) = index.find_top_level_kw(last.clone(), "USING")
    {
        last.end = using_idx;
    }
    let mut out_bytes = Vec::new();
    for arg in args {
        budget.charge_step()?;
        let arg = index.strip_wrapping_parens(arg);
        match &index.toks[arg] {
            [CredTok::Word(w)] if w == "NULL" => {}
            [CredTok::Number(num_str)] => {
                let val: u64 = num_str.parse().ok()?;
                if val == 0 {
                    if out_bytes.len() >= MAX_CRED_EXPR_BYTES {
                        budget.mark_exhausted();
                        return None;
                    }
                    out_bytes.push(0);
                } else {
                    let be = val.to_be_bytes();
                    let first_nz = be.iter().position(|&b| b != 0).unwrap_or(be.len());
                    let chunk = &be[first_nz..];
                    if out_bytes
                        .len()
                        .checked_add(chunk.len())
                        .is_none_or(|n| n > MAX_CRED_EXPR_BYTES)
                    {
                        budget.mark_exhausted();
                        return None;
                    }
                    out_bytes.extend_from_slice(chunk);
                }
            }
            [CredTok::HexLiteral(hex)] => {
                let chunk = decode_hex_sql_bytes(hex, budget)?;
                if out_bytes
                    .len()
                    .checked_add(chunk.len())
                    .is_none_or(|n| n > MAX_CRED_EXPR_BYTES)
                {
                    budget.mark_exhausted();
                    return None;
                }
                out_bytes.extend_from_slice(&chunk);
            }
            _ => return None,
        }
    }
    let s = String::from_utf8_lossy(&out_bytes).into_owned();
    if s.len() > MAX_CRED_EXPR_BYTES {
        budget.mark_exhausted();
        return None;
    }
    budget.charge_bytes(s.len())?;
    Some(vec![s])
}

fn eval_mysql_trim_candidates(
    index: &CredTokIndex<'_>,
    inner: std::ops::Range<usize>,
    opts: MysqlCredEvalOpts,
    depth: usize,
    budget: &mut CredEvalBudget,
) -> Option<Vec<String>> {
    #[derive(Clone, Copy)]
    enum TrimDir {
        Both,
        Leading,
        Trailing,
    }
    let (dir, rem_range, target_range) =
        if let Some(from_idx) = index.find_top_level_kw(inner.clone(), "FROM") {
            let spec_range = inner.start..from_idx;
            let (dir, rem) = match &index.toks[spec_range.clone()] {
                [CredTok::Word(k), ..] if k == "LEADING" => {
                    (TrimDir::Leading, (spec_range.start + 1)..spec_range.end)
                }
                [CredTok::Word(k), ..] if k == "TRAILING" => {
                    (TrimDir::Trailing, (spec_range.start + 1)..spec_range.end)
                }
                [CredTok::Word(k), ..] if k == "BOTH" => {
                    (TrimDir::Both, (spec_range.start + 1)..spec_range.end)
                }
                _ => (TrimDir::Both, spec_range),
            };
            (dir, (!rem.is_empty()).then_some(rem), (from_idx + 1)..inner.end)
        } else {
            (TrimDir::Both, None, inner)
        };
    let targets = mysql_eval_cred_sql_expr_candidates(index, target_range, opts, depth + 1, budget)?;
    let rems = if let Some(tok_range) = rem_range {
        mysql_eval_cred_sql_expr_candidates(index, tok_range, opts, depth + 1, budget)?
    } else {
        vec![" ".to_owned()]
    };
    let mut out = Vec::new();
    for t in &targets {
        for r in &rems {
            budget.charge_step()?;
            let mut s = t.as_str();
            if !r.is_empty() {
                if matches!(dir, TrimDir::Both | TrimDir::Leading) {
                    while let Some(rest) = s.strip_prefix(r.as_str()) {
                        s = rest;
                    }
                }
                if matches!(dir, TrimDir::Both | TrimDir::Trailing) {
                    while let Some(rest) = s.strip_suffix(r.as_str()) {
                        s = rest;
                    }
                }
            }
            push_unique_cred_candidate(&mut out, s.to_owned(), budget)?;
        }
    }
    (!out.is_empty()).then_some(out)
}

fn map_cred_candidates(
    vals: Vec<String>,
    budget: &mut CredEvalBudget,
    f: impl Fn(String) -> Option<String>,
) -> Option<Vec<String>> {
    let mut out = Vec::with_capacity(vals.len());
    for val in vals {
        budget.charge_step()?;
        let transformed = f(val)?;
        push_unique_cred_candidate(&mut out, transformed, budget)?;
    }
    (!out.is_empty()).then_some(out)
}

fn apply_mysql_cast_type(val: String, type_toks: &[CredTok]) -> Option<String> {
    let [CredTok::Word(ty), rest @ ..] = type_toks else {
        return None;
    };
    match ty.as_str() {
        "CHAR" | "NCHAR" => match rest {
            [CredTok::Punct('('), CredTok::Number(n_str), CredTok::Punct(')'), ..] => {
                let n: usize = n_str.parse().ok()?;
                Some(val.chars().take(n).collect())
            }
            _ if !matches!(rest.first(), Some(CredTok::Punct('('))) => Some(val),
            _ => None,
        },
        "BINARY" => match rest {
            [] => Some(val),
            [CredTok::Punct('('), CredTok::Number(n_str), CredTok::Punct(')')] => {
                let n: usize = n_str.parse().ok()?;
                if n <= val.len() {
                    Some(String::from_utf8_lossy(&val.as_bytes()[..n]).into_owned())
                } else {
                    None
                }
            }
            _ => None,
        },
        _ => None,
    }
}

fn mysql_eval_cred_sql_expr_candidates(
    index: &CredTokIndex<'_>,
    range: std::ops::Range<usize>,
    opts: MysqlCredEvalOpts,
    depth: usize,
    budget: &mut CredEvalBudget,
) -> Option<Vec<String>> {
    budget.charge_step()?;
    if depth >= MAX_SQL_MODE_EXPR_DEPTH {
        budget.mark_exhausted();
        return None;
    }
    let range = index.strip_wrapping_parens(range);
    if range.is_empty() {
        return None;
    }
    let toks = &index.toks[range.clone()];
    if range.len() >= 2
        && matches!(&toks[0], CredTok::Word(c) if c == "CASE")
        && matches!(&toks[range.len() - 1], CredTok::Word(e) if e == "END")
        && let Some(branches) =
            extract_cred_case_branches(index, (range.start + 1)..(range.end - 1))
    {
        return eval_cred_branches(index, &branches, opts, depth, budget);
    }
    if range.len() >= 3
        && let CredTok::Word(fn_name) = &toks[0]
        && index.is_single_call(range.start + 1, range.end - 1)
    {
        let inner = (range.start + 2)..(range.end - 1);
        match fn_name.as_str() {
            "CONCAT" => {
                let args = index.split_top_level_commas(inner);
                return eval_mysql_concat_args(index, &args, None, opts, depth, budget);
            }
            "CONCAT_WS" => {
                let args = index.split_top_level_commas(inner);
                let (sep_range, rest_args) = args.split_first()?;
                let sep_opts = if opts.strict_concat {
                    MysqlCredEvalOpts {
                        allow_unresolved_non_literal_branch: false,
                        ..opts
                    }
                } else {
                    opts
                };
                let seps = mysql_eval_cred_sql_expr_candidates(
                    index,
                    sep_range.clone(),
                    sep_opts,
                    depth + 1,
                    budget,
                )?;
                return eval_mysql_concat_args(index, rest_args, Some(seps), opts, depth, budget);
            }
            "CHAR" => return eval_mysql_char_candidates(index, inner, budget),
            "QUOTE" => {
                let vals =
                    mysql_eval_cred_sql_expr_candidates(index, inner, opts, depth + 1, budget)?;
                let mut out = Vec::with_capacity(vals.len());
                for val in vals {
                    budget.charge_step()?;
                    let quote_count = val.bytes().filter(|&b| b == b'\'').count();
                    let Some(needed) = val.len().checked_add(2).and_then(|n| n.checked_add(quote_count)) else {
                        budget.mark_exhausted();
                        return None;
                    };
                    if needed > MAX_CRED_EXPR_BYTES {
                        budget.mark_exhausted();
                        return None;
                    }
                    let escaped = val.replace('\'', "''");
                    push_unique_cred_candidate(&mut out, format!("'{escaped}'"), budget)?;
                }
                return (!out.is_empty()).then_some(out);
            }
            "UNHEX" => {
                let hexs =
                    mysql_eval_cred_sql_expr_candidates(index, inner, opts, depth + 1, budget)?;
                let mut out = Vec::with_capacity(hexs.len());
                for hex in hexs {
                    budget.charge_step()?;
                    let decoded = decode_hex_sql_string(&hex, budget)?;
                    push_unique_cred_candidate(&mut out, decoded, budget)?;
                }
                return (!out.is_empty()).then_some(out);
            }
            "CAST" => {
                let as_idx = index.find_top_level_kw(inner.clone(), "AS")?;
                let vals = mysql_eval_cred_sql_expr_candidates(
                    index,
                    inner.start..as_idx,
                    opts,
                    depth + 1,
                    budget,
                )?;
                return map_cred_candidates(vals, budget, |val| {
                    apply_mysql_cast_type(val, &index.toks[(as_idx + 1)..inner.end])
                });
            }
            "CONVERT" => {
                let mut idx = inner.start;
                let mut sep_idx = None;
                while idx < inner.end {
                    match &index.toks[idx] {
                        CredTok::Punct('(' | '[') => {
                            idx = index.skip_balanced(idx, inner.end);
                            continue;
                        }
                        CredTok::Word(w) if w == "USING" => {
                            sep_idx = Some(idx);
                            break;
                        }
                        CredTok::Punct(',') => {
                            sep_idx = Some(idx);
                            break;
                        }
                        _ => {}
                    }
                    idx += 1;
                }
                let sep_idx = sep_idx?;
                let vals = mysql_eval_cred_sql_expr_candidates(
                    index,
                    inner.start..sep_idx,
                    opts,
                    depth + 1,
                    budget,
                )?;
                if matches!(index.toks.get(sep_idx), Some(CredTok::Punct(','))) {
                    return map_cred_candidates(vals, budget, |val| {
                        apply_mysql_cast_type(val, &index.toks[(sep_idx + 1)..inner.end])
                    });
                }
                return Some(vals);
            }
            "LOWER" | "LCASE" => {
                let vals =
                    mysql_eval_cred_sql_expr_candidates(index, inner, opts, depth + 1, budget)?;
                return map_cred_candidates(vals, budget, |s| Some(s.to_ascii_lowercase()));
            }
            "UPPER" | "UCASE" => {
                let vals =
                    mysql_eval_cred_sql_expr_candidates(index, inner, opts, depth + 1, budget)?;
                return map_cred_candidates(vals, budget, |s| Some(s.to_ascii_uppercase()));
            }
            "LTRIM" => {
                let vals =
                    mysql_eval_cred_sql_expr_candidates(index, inner, opts, depth + 1, budget)?;
                return map_cred_candidates(vals, budget, |s| {
                    Some(s.trim_start_matches(' ').to_owned())
                });
            }
            "RTRIM" => {
                let vals =
                    mysql_eval_cred_sql_expr_candidates(index, inner, opts, depth + 1, budget)?;
                return map_cred_candidates(vals, budget, |s| {
                    Some(s.trim_end_matches(' ').to_owned())
                });
            }
            "TRIM" => return eval_mysql_trim_candidates(index, inner, opts, depth, budget),
            "REPLACE" => {
                let args = index.split_top_level_commas(inner);
                if args.len() != 3 {
                    return None;
                }
                let haystacks = mysql_eval_cred_sql_expr_candidates(
                    index,
                    args[0].clone(),
                    opts,
                    depth + 1,
                    budget,
                )?;
                let froms = mysql_eval_cred_sql_expr_candidates(
                    index,
                    args[1].clone(),
                    opts,
                    depth + 1,
                    budget,
                )?;
                let tos = mysql_eval_cred_sql_expr_candidates(
                    index,
                    args[2].clone(),
                    opts,
                    depth + 1,
                    budget,
                )?;
                let mut out = Vec::new();
                for h in &haystacks {
                    for f in &froms {
                        for t in &tos {
                            budget.charge_step()?;
                            let replaced =
                                bounded_replace(h, f, t, MAX_CRED_EXPR_BYTES, budget)?;
                            push_unique_cred_candidate(&mut out, replaced, budget)?;
                        }
                    }
                }
                return (!out.is_empty()).then_some(out);
            }
            "IF" => {
                let args = index.split_top_level_commas(inner);
                if args.len() != 3 {
                    return None;
                }
                return eval_cred_branches(
                    index,
                    &[args[1].clone(), args[2].clone()],
                    opts,
                    depth,
                    budget,
                );
            }
            "IFNULL" | "NULLIF" | "COALESCE" => {
                let args = index.split_top_level_commas(inner);
                if args.is_empty() {
                    return None;
                }
                return eval_cred_branches(index, &args, opts, depth, budget);
            }
            "ELT" => {
                let args = index.split_top_level_commas(inner);
                let (_, rest) = args.split_first()?;
                if rest.is_empty() {
                    return None;
                }
                return eval_cred_branches(index, rest, opts, depth, budget);
            }
            _ => return None,
        }
    }

    let mut out = String::new();
    let mut saw_atom = false;
    let mut idx = 0usize;
    while idx < toks.len() {
        let mut cur = idx;
        if let Some(CredTok::Word(w)) = toks.get(cur)
            && w.starts_with('_')
        {
            cur += 1;
        }
        let atom = match toks.get(cur)? {
            CredTok::Literal(s) => s.clone(),
            CredTok::DoubleQuoted(s) if !opts.ansi_quotes => s.clone(),
            CredTok::HexLiteral(h) => decode_hex_sql_string(h, budget)?,
            _ => return None,
        };
        if out
            .len()
            .checked_add(atom.len())
            .is_none_or(|n| n > MAX_CRED_EXPR_BYTES)
        {
            budget.mark_exhausted();
            return None;
        }
        budget.charge_bytes(atom.len())?;
        saw_atom = true;
        out.push_str(&atom);
        idx = cur + 1;
        if matches!(toks.get(idx), Some(CredTok::Word(w)) if w == "COLLATE")
            && matches!(
                toks.get(idx + 1),
                Some(CredTok::Word(_) | CredTok::QuotedIdent(_) | CredTok::DoubleQuoted(_))
            )
        {
            idx += 2;
        }
    }
    saw_atom.then(|| vec![out])
}

fn is_pg_secret_guc_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "PRIMARY_CONNINFO" | "SSL_PASSPHRASE_COMMAND"
    ) || upper == "PASSWORD"
        || upper.ends_with("_PASSWORD")
        || upper == "PASSWD"
        || upper.ends_with("_PASSWD")
}

fn is_pg_guc_secret_value(rest: &[CredTok]) -> bool {
    let vals = match rest {
        [CredTok::Punct('='), vals @ ..] => vals,
        [CredTok::Word(op), vals @ ..] if op == "TO" => vals,
        _ => return false,
    };
    let Some(first) = vals.first() else {
        return false;
    };
    if matches!(first, CredTok::Word(w) if w == "DEFAULT") {
        return false;
    }
    matches!(
        first,
        CredTok::Literal(_)
            | CredTok::TypedLiteral(_)
            | CredTok::HexLiteral(_)
            | CredTok::DoubleQuoted(_)
            | CredTok::Word(_)
            | CredTok::Number(_)
            | CredTok::Other
    )
}

fn parse_pg_guc_name(toks: &[CredTok]) -> Option<(&str, &[CredTok])> {
    let (first, mut rest) = match toks {
        [CredTok::Word(w) | CredTok::DoubleQuoted(w), rest @ ..] => (w.as_str(), rest),
        _ => return None,
    };
    let mut param_name = first.rsplit('.').next().unwrap_or(first);
    while let [
        CredTok::Punct('.'),
        CredTok::Word(next) | CredTok::DoubleQuoted(next),
        tail @ ..,
    ] = rest
    {
        param_name = next.rsplit('.').next().unwrap_or(next.as_str());
        rest = tail;
    }
    Some((param_name, rest))
}

fn pg_clause_sets_credential_guc(assign: &[CredTok]) -> bool {
    let Some((param_name, rest)) = parse_pg_guc_name(assign) else {
        return false;
    };
    is_pg_secret_guc_name(param_name) && is_pg_guc_secret_value(rest)
}

fn pg_sets_credential_guc(toks: &[CredTok]) -> bool {
    match toks {
        [CredTok::Word(a), CredTok::Word(b), CredTok::Word(c), rest @ ..]
            if a == "ALTER" && b == "SYSTEM" && c == "SET" =>
        {
            return pg_clause_sets_credential_guc(rest);
        }
        [CredTok::Word(a), CredTok::Word(b), rest @ ..]
            if a == "SET" && (b == "SESSION" || b == "LOCAL") =>
        {
            return pg_clause_sets_credential_guc(rest);
        }
        [CredTok::Word(a), rest @ ..] if a == "SET" => {
            return pg_clause_sets_credential_guc(rest);
        }
        _ => {}
    }

    let scan_tail = match toks {
        [CredTok::Word(a), CredTok::Word(b), rest @ ..]
            if a == "ALTER"
                && matches!(
                    b.as_str(),
                    "DATABASE" | "ROLE" | "USER" | "FUNCTION" | "PROCEDURE" | "ROUTINE"
                ) =>
        {
            Some(rest)
        }
        [CredTok::Word(a), CredTok::Word(b), rest @ ..]
            if a == "CREATE" && (b == "FUNCTION" || b == "PROCEDURE") =>
        {
            Some(rest)
        }
        [
            CredTok::Word(a),
            CredTok::Word(b),
            CredTok::Word(c),
            CredTok::Word(d),
            rest @ ..,
        ] if a == "CREATE"
            && b == "OR"
            && c == "REPLACE"
            && (d == "FUNCTION" || d == "PROCEDURE") =>
        {
            Some(rest)
        }
        _ => None,
    };

    let Some(rest) = scan_tail else {
        return false;
    };
    let mut depth = 0usize;
    let mut bracket_depth = 0usize;
    for (idx, tok) in rest.iter().enumerate() {
        match tok {
            CredTok::Punct('(') => depth += 1,
            CredTok::Punct(')') => depth = depth.saturating_sub(1),
            CredTok::Punct('[') => bracket_depth += 1,
            CredTok::Punct(']') => bracket_depth = bracket_depth.saturating_sub(1),
            CredTok::Word(w)
                if depth == 0
                    && bracket_depth == 0
                    && w == "SET"
                    && pg_clause_sets_credential_guc(&rest[idx + 1..]) =>
            {
                return true;
            }
            _ => {}
        }
    }
    false
}

fn is_pg_catalog_schema_tok(tok: &CredTok) -> bool {
    match tok {
        CredTok::Word(s) => s == "PG_CATALOG",
        CredTok::DoubleQuoted(s) => s == "pg_catalog",
        _ => false,
    }
}

fn is_pg_unbounded_text_type_toks(toks: &[CredTok], allow_multiword_sql: bool) -> bool {
    match toks {
        [CredTok::Word(w)] => w == "TEXT" || w == "VARCHAR",
        [CredTok::DoubleQuoted(s)] => s == "text" || s == "varchar",
        [CredTok::Word(c), CredTok::Word(v)] if allow_multiword_sql => {
            c == "CHARACTER" && v == "VARYING"
        }
        _ => false,
    }
}

fn is_pg_non_truncating_text_cast(type_toks: &[CredTok]) -> bool {
    let (has_schema, rest) = match type_toks {
        [schema, CredTok::Punct('.'), tail @ ..] if is_pg_catalog_schema_tok(schema) => {
            (true, tail)
        }
        _ => (false, type_toks),
    };
    is_pg_unbounded_text_type_toks(rest, !has_schema)
        || matches!(rest, [CredTok::Word(ty)] if ty == "UNKNOWN")
        || matches!(rest, [CredTok::DoubleQuoted(ty)] if ty == "unknown")
}

fn is_pg_catalog_non_truncating_text_cast(type_toks: &[CredTok]) -> bool {
    let [schema, CredTok::Punct('.'), rest @ ..] = type_toks else {
        return false;
    };
    if !is_pg_catalog_schema_tok(schema) {
        return false;
    }
    is_pg_unbounded_text_type_toks(rest, false)
        || matches!(rest, [CredTok::Word(ty)] if ty == "UNKNOWN")
        || matches!(rest, [CredTok::DoubleQuoted(ty)] if ty == "unknown")
}

fn pg_expr_is_null(
    index: &CredTokIndex<'_>,
    range: std::ops::Range<usize>,
    depth: usize,
) -> bool {
    if depth >= MAX_SQL_MODE_EXPR_DEPTH {
        return false;
    }
    let range = index.strip_wrapping_parens(range);
    if range.is_empty() {
        return false;
    }
    if matches!(&index.toks[range.clone()], [CredTok::Word(w)] if w == "NULL") {
        return true;
    }
    if let Some((expr_range, ty_range)) = index.pg_split_last_top_level_type_cast(range.clone()) {
        return is_pg_non_truncating_text_cast(&index.toks[ty_range])
            && pg_expr_is_null(index, expr_range, depth + 1);
    }
    if range.len() >= 3
        && matches!(&index.toks[range.start], CredTok::Word(fn_name) if fn_name == "CAST")
        && index.is_single_call(range.start + 1, range.end - 1)
    {
        let inner = (range.start + 2)..(range.end - 1);
        if let Some(as_idx) = index.find_top_level_kw(inner.clone(), "AS") {
            return is_pg_non_truncating_text_cast(&index.toks[(as_idx + 1)..inner.end])
                && pg_expr_is_null(index, inner.start..as_idx, depth + 1);
        }
    }
    false
}

/// Evaluates only plain PostgreSQL string literals (with optional wrapping parens,
/// SQL-standard adjacent literals, or schema-qualified `pg_catalog` identity casts).
/// Unqualified type names, `N'...'` / `B'...'` typed literals, and any computed expression
/// return `None` so callers fail closed.
fn pg_eval_simple_string_literal(
    index: &CredTokIndex<'_>,
    mut range: std::ops::Range<usize>,
) -> Option<String> {
    let mut peel_steps = 0usize;
    loop {
        if peel_steps >= MAX_SQL_MODE_EXPR_DEPTH {
            return None;
        }
        peel_steps += 1;
        let prev = range.clone();
        range = index.strip_wrapping_parens(range);
        if range.is_empty() {
            return None;
        }
        if let Some((expr_range, type_range)) =
            index.pg_split_last_top_level_type_cast(range.clone())
        {
            if is_pg_catalog_non_truncating_text_cast(&index.toks[type_range]) {
                range = expr_range;
                continue;
            }
            return None;
        }
        if range.len() >= 3
            && matches!(&index.toks[range.start], CredTok::Word(fn_name) if fn_name == "CAST")
            && index.is_single_call(range.start + 1, range.end - 1)
        {
            let inner = (range.start + 2)..(range.end - 1);
            if let Some(as_idx) = index.find_top_level_kw(inner.clone(), "AS")
                && is_pg_catalog_non_truncating_text_cast(&index.toks[(as_idx + 1)..inner.end])
            {
                range = inner.start..as_idx;
                continue;
            }
            return None;
        }
        if range.len() >= 2
            && matches!(index.toks[range.end - 1], CredTok::Literal(_))
            && is_pg_catalog_non_truncating_text_cast(&index.toks[range.start..(range.end - 1)])
        {
            range = (range.end - 1)..range.end;
            continue;
        }
        if range == prev {
            break;
        }
    }
    let slice = &index.toks[range];
    if slice.is_empty() {
        return None;
    }
    let mut total_len = 0usize;
    for tok in slice {
        let CredTok::Literal(s) = tok else {
            return None;
        };
        total_len = total_len.checked_add(s.len())?;
        if total_len > MAX_CRED_EXPR_BYTES {
            return None;
        }
    }
    let mut out = String::with_capacity(total_len);
    for tok in slice {
        if let CredTok::Literal(s) = tok {
            out.push_str(s);
        }
    }
    Some(out)
}

fn pg_calls_set_config_secret(index: &CredTokIndex<'_>) -> bool {
    visit_set_config_calls(index.toks, |call| {
        if pg_expr_is_null(index, call.value, 0) {
            return std::ops::ControlFlow::Continue(());
        }
        match pg_eval_simple_string_literal(index, call.setting) {
            Some(name) => {
                let last_seg = name.rsplit('.').next().unwrap_or(&name);
                if is_pg_secret_guc_name(last_seg) {
                    return std::ops::ControlFlow::Break(true);
                }
                std::ops::ControlFlow::Continue(())
            }
            None => std::ops::ControlFlow::Break(true),
        }
    })
    .unwrap_or(false)
}

fn mysql_set_user_var_rhs_offset(slice: &[CredTok]) -> Option<usize> {
    if !matches!(slice.first(), Some(CredTok::Punct('@'))) {
        return None;
    }
    let after_name = match slice.get(1)? {
        CredTok::QuotedIdent(_) | CredTok::DoubleQuoted(_) | CredTok::Literal(_) => 2,
        CredTok::Word(_) | CredTok::Number(_) | CredTok::HexLiteral(_) | CredTok::Punct('.') => {
            let mut idx = 2;
            while matches!(
                slice.get(idx),
                Some(
                    CredTok::Word(_)
                        | CredTok::Number(_)
                        | CredTok::HexLiteral(_)
                        | CredTok::Punct('.')
                )
            ) {
                idx += 1;
            }
            idx
        }
        _ => return None,
    };
    match slice.get(after_name..) {
        Some([CredTok::Punct('='), ..]) => Some(after_name + 1),
        Some([CredTok::Punct(':'), CredTok::Punct('='), ..]) => Some(after_name + 2),
        _ => None,
    }
}

const MAX_CRED_RECURSION_DEPTH: usize = 4;

fn statement_sets_credentials(
    engine: DbEngine,
    mut toks: &[CredTok],
    mysql_mode: MysqlQuoteMode,
    outer_mode_known: bool,
    allow_placeholder: bool,
    depth: usize,
    budget: &mut CredEvalBudget,
) -> bool {
    if engine == DbEngine::Mysql {
        toks = strip_mariadb_cred_prefixes(toks);
    }
    if toks.is_empty() {
        return false;
    }
    if depth == 0 {
        budget.begin_stmt();
    }

    if engine == DbEngine::Mysql {
        let inner_mode = outer_mode_known.then_some(mysql_mode);
        if let [
            CredTok::Word(p),
            CredTok::Word(_) | CredTok::QuotedIdent(_) | CredTok::DoubleQuoted(_),
            CredTok::Word(from),
            expr @ ..,
        ] = toks
            && p == "PREPARE"
            && from == "FROM"
        {
            let may_produce_sql = mysql_expr_may_produce_sql(expr, mysql_mode.ansi_quotes);
            if depth >= MAX_CRED_RECURSION_DEPTH {
                if may_produce_sql {
                    budget.mark_exhausted();
                    return true;
                }
            } else {
                let index = CredTokIndex::new(toks);
                match mysql_eval_cred_sql_expr_candidates(
                    &index,
                    3..toks.len(),
                    MysqlCredEvalOpts {
                        ansi_quotes: mysql_mode.ansi_quotes,
                        allow_unresolved_non_literal_branch: true,
                        strict_concat: true,
                    },
                    0,
                    budget,
                ) {
                    Some(cands) => {
                        for inner_sql in cands {
                            if !mysql_inner_sql_may_set_credentials(&inner_sql) {
                                continue;
                            }
                            if budget.charge_recursive_call().is_none() {
                                return true;
                            }
                            if sql_sets_credentials_with_session_mode_depth(
                                DbEngine::Mysql,
                                &inner_sql,
                                SqlSessionMode::mysql(inner_mode),
                                false,
                                depth + 1,
                                budget,
                            ) {
                                return true;
                            }
                        }
                    }
                    None if may_produce_sql || budget.is_exhausted() => return true,
                    None => {}
                }
            }
        }
        if matches!(
            toks,
            [CredTok::Word(exec), CredTok::Word(imm), ..] if exec == "EXECUTE" && imm == "IMMEDIATE"
        ) {
            let index = CredTokIndex::new(toks);
            let rest_range = 2..toks.len();
            let using_idx = index.find_top_level_kw(rest_range.clone(), "USING");
            let expr_range = using_idx.map_or(rest_range, |idx| 2..idx);
            let expr = &toks[expr_range.clone()];
            let has_using_literal = using_idx.is_some_and(|idx| {
                toks[idx + 1..]
                    .iter()
                    .any(|t| is_cred_value_tok(DbEngine::Mysql, t, false))
            });
            let may_produce_sql =
                mysql_expr_may_produce_sql(expr, mysql_mode.ansi_quotes) || has_using_literal;
            if depth >= MAX_CRED_RECURSION_DEPTH {
                if may_produce_sql {
                    budget.mark_exhausted();
                    return true;
                }
            } else {
                match mysql_eval_cred_sql_expr_candidates(
                    &index,
                    expr_range,
                    MysqlCredEvalOpts {
                        ansi_quotes: mysql_mode.ansi_quotes,
                        allow_unresolved_non_literal_branch: !has_using_literal,
                        strict_concat: true,
                    },
                    0,
                    budget,
                ) {
                    Some(cands) => {
                        for inner_sql in cands {
                            if !mysql_inner_sql_may_set_credentials(&inner_sql) {
                                continue;
                            }
                            if budget.charge_recursive_call().is_none() {
                                return true;
                            }
                            if sql_sets_credentials_with_session_mode_depth(
                                DbEngine::Mysql,
                                &inner_sql,
                                SqlSessionMode::mysql(inner_mode),
                                has_using_literal,
                                depth + 1,
                                budget,
                            ) {
                                return true;
                            }
                        }
                    }
                    None if may_produce_sql || budget.is_exhausted() => return true,
                    None => {}
                }
            }
        }
        if matches!(toks.first(), Some(CredTok::Word(w)) if w == "SET")
            && toks[1..].iter().any(|t| matches!(t, CredTok::Punct('@')))
        {
            let index = CredTokIndex::new(toks);
            for assign_range in index.split_top_level_commas(1..toks.len()) {
                let Some(offset) = mysql_set_user_var_rhs_offset(&toks[assign_range.clone()]) else {
                    continue;
                };
                let rhs_range =
                    index.strip_wrapping_parens((assign_range.start + offset)..assign_range.end);
                let direct_literal = match &toks[rhs_range.clone()] {
                    [CredTok::Literal(s)] => Some(s.as_str()),
                    [CredTok::DoubleQuoted(s)] if !mysql_mode.ansi_quotes => Some(s.as_str()),
                    _ => None,
                };
                if let Some(inner_sql) = direct_literal {
                    if !mysql_inner_sql_may_set_credentials(inner_sql) {
                        continue;
                    }
                    if depth >= MAX_CRED_RECURSION_DEPTH {
                        budget.mark_exhausted();
                        return true;
                    }
                    if budget.charge_recursive_call().is_none() {
                        return true;
                    }
                    if sql_sets_credentials_with_session_mode_depth(
                        DbEngine::Mysql,
                        inner_sql,
                        SqlSessionMode::mysql(inner_mode),
                        false,
                        depth + 1,
                        budget,
                    ) {
                        return true;
                    }
                    continue;
                }
                if !mysql_expr_may_produce_sql(&toks[rhs_range.clone()], mysql_mode.ansi_quotes) {
                    continue;
                }
                if depth >= MAX_CRED_RECURSION_DEPTH {
                    budget.mark_exhausted();
                    return true;
                }
                match mysql_eval_cred_sql_expr_candidates(
                    &index,
                    rhs_range,
                    MysqlCredEvalOpts {
                        ansi_quotes: mysql_mode.ansi_quotes,
                        allow_unresolved_non_literal_branch: true,
                        strict_concat: false,
                    },
                    0,
                    budget,
                ) {
                    Some(cands) => {
                        for inner_sql in cands {
                            if !mysql_inner_sql_may_set_credentials(&inner_sql) {
                                continue;
                            }
                            if budget.charge_recursive_call().is_none() {
                                return true;
                            }
                            if sql_sets_credentials_with_session_mode_depth(
                                DbEngine::Mysql,
                                &inner_sql,
                                SqlSessionMode::mysql(inner_mode),
                                false,
                                depth + 1,
                                budget,
                            ) {
                                return true;
                            }
                        }
                    }
                    None if budget.is_exhausted() => return true,
                    None => {}
                }
            }
        }
    }

    if engine == DbEngine::Postgres {
        if pg_sets_credential_guc(toks) {
            return true;
        }
        if toks.iter().any(|tok| {
            matches!(tok, CredTok::Word(f) | CredTok::DoubleQuoted(f) if f.eq_ignore_ascii_case("SET_CONFIG"))
        }) {
            let index = CredTokIndex::new(toks);
            if pg_calls_set_config_secret(&index) {
                return true;
            }
        }
    }

    if engine == DbEngine::Sqlite {
        let is_pragma_key = match toks {
            [CredTok::Word(p), key_tok, CredTok::Punct('=' | '('), ..] if p == "PRAGMA" => {
                is_sqlite_key_pragma_name(key_tok)
            }
            [
                CredTok::Word(p),
                _,
                CredTok::Punct('.'),
                key_tok,
                CredTok::Punct('=' | '('),
                ..,
            ] if p == "PRAGMA" => is_sqlite_key_pragma_name(key_tok),
            _ => false,
        };
        if is_pragma_key || sqlite_attach_sets_credentials(toks) {
            return true;
        }
    }

    if matches!(
        toks,
        [CredTok::Word(a), CredTok::Word(b), ..] if a == "SET" && b == "PASSWORD"
    ) && toks[2..]
        .iter()
        .any(|t| is_cred_value_tok(engine, t, allow_placeholder))
    {
        return true;
    }

    if matches!(toks.first(), Some(CredTok::Word(w)) if w == "GRANT")
        && contains_identified_secret(engine, toks, allow_placeholder)
    {
        return true;
    }

    if engine == DbEngine::Postgres
        && matches!(
            toks,
            [CredTok::Word(verb), CredTok::Word(sub), ..]
                if (verb == "CREATE" || verb == "ALTER") && sub == "SUBSCRIPTION"
        )
        && toks[2..].windows(2).any(|win| {
            matches!(&win[0], CredTok::Word(k) if k == "CONNECTION")
                && is_cred_value_tok(engine, &win[1], allow_placeholder)
        })
    {
        return true;
    }

    let role_or_user_body = match toks {
        [CredTok::Word(verb), CredTok::Word(kind), rest @ ..]
            if (verb == "CREATE" || verb == "ALTER")
                && (kind == "ROLE"
                    || kind == "USER"
                    || (engine == DbEngine::Postgres && kind == "GROUP"))
                && !matches!(rest.first(), Some(CredTok::Word(w)) if w == "MAPPING") =>
        {
            Some((kind.as_str(), rest))
        }
        [
            CredTok::Word(a),
            CredTok::Word(b),
            CredTok::Word(c),
            CredTok::Word(kind),
            rest @ ..,
        ] if a == "CREATE"
            && b == "OR"
            && c == "REPLACE"
            && (kind == "ROLE" || kind == "USER")
            && !matches!(rest.first(), Some(CredTok::Word(w)) if w == "MAPPING") =>
        {
            Some((kind.as_str(), rest))
        }
        _ => None,
    };

    if let Some((kind, rest)) = role_or_user_body {
        if contains_password_literal(engine, rest, allow_placeholder)
            || contains_identified_secret(engine, rest, allow_placeholder)
        {
            return true;
        }
        if kind == "USER"
            && rest.windows(2).any(|win| {
                matches!(&win[0], CredTok::Word(w) if w == "REPLACE")
                    && is_cred_value_tok(engine, &win[1], allow_placeholder)
            })
        {
            return true;
        }
    }

    let mapping_or_server_body = match toks {
        [CredTok::Word(verb), CredTok::Word(u), CredTok::Word(m), rest @ ..]
            if (verb == "CREATE" || verb == "ALTER") && u == "USER" && m == "MAPPING" =>
        {
            Some(rest)
        }
        [CredTok::Word(verb), CredTok::Word(s), rest @ ..]
            if (verb == "CREATE" || verb == "ALTER") && s == "SERVER" =>
        {
            Some(rest)
        }
        [
            CredTok::Word(a),
            CredTok::Word(b),
            CredTok::Word(c),
            CredTok::Word(s),
            rest @ ..,
        ] if a == "CREATE" && b == "OR" && c == "REPLACE" && s == "SERVER" => Some(rest),
        _ => None,
    };

    if let Some(rest) = mapping_or_server_body {
        return contains_fdw_secret_option(engine, rest, allow_placeholder);
    }

    if engine == DbEngine::Mysql {
        let repl_body = match toks {
            [
                CredTok::Word(a),
                CredTok::Word(b),
                CredTok::Word(c),
                CredTok::Word(d),
                rest @ ..,
            ] if a == "CHANGE" && b == "REPLICATION" && c == "SOURCE" && d == "TO" => Some(rest),
            [CredTok::Word(a), CredTok::Word(b), rest @ ..] if a == "CHANGE" && b == "MASTER" => {
                Some(rest)
            }
            _ => None,
        };
        if let Some(rest) = repl_body
            && contains_replication_secret_option(engine, rest, allow_placeholder)
        {
            return true;
        }
        if matches!(
            toks,
            [CredTok::Word(a), CredTok::Word(b), ..]
                if a == "START"
                    && (b == "REPLICA" || b == "SLAVE" || b == "GROUP_REPLICATION")
        ) && contains_password_literal(engine, &toks[2..], allow_placeholder)
        {
            return true;
        }
        if matches!(
            toks,
            [CredTok::Word(a), CredTok::Word(b), ..] if a == "CLONE" && b == "INSTANCE"
        ) && contains_identified_secret(engine, &toks[2..], allow_placeholder)
        {
            return true;
        }
    }

    false
}

fn contains_fdw_secret_option(engine: DbEngine, toks: &[CredTok], allow_placeholder: bool) -> bool {
    for idx in 0..toks.len() {
        let (CredTok::Word(w) | CredTok::DoubleQuoted(w)) = &toks[idx] else {
            continue;
        };
        let upper = w.to_ascii_uppercase();
        let is_secret_key = upper == "PASSWORD"
            || upper.ends_with("PASSWORD")
            || upper == "PASSWD"
            || upper == "SECRET"
            || upper.ends_with("_SECRET");
        if !is_secret_key {
            continue;
        }
        if toks
            .get(idx + 1)
            .is_some_and(|t| is_cred_value_tok(engine, t, allow_placeholder))
            || (matches!(toks.get(idx + 1), Some(CredTok::Punct('=')))
                && toks
                    .get(idx + 2)
                    .is_some_and(|t| is_cred_value_tok(engine, t, allow_placeholder)))
        {
            return true;
        }
    }
    false
}

fn contains_replication_secret_option(
    engine: DbEngine,
    toks: &[CredTok],
    allow_placeholder: bool,
) -> bool {
    for idx in 0..toks.len() {
        let CredTok::Word(w) = &toks[idx] else {
            continue;
        };
        if w != "PASSWORD" && !w.ends_with("_PASSWORD") {
            continue;
        }
        if toks
            .get(idx + 1)
            .is_some_and(|t| is_cred_value_tok(engine, t, allow_placeholder))
            || (matches!(toks.get(idx + 1), Some(CredTok::Punct('=')))
                && toks
                    .get(idx + 2)
                    .is_some_and(|t| is_cred_value_tok(engine, t, allow_placeholder)))
        {
            return true;
        }
    }
    false
}

/// Returns `true` if any SQL statement in `sql` sets a role, user, FDW, or encryption credential.
pub fn sql_sets_credentials(engine: DbEngine, sql: &str) -> bool {
    sql_sets_credentials_with_mysql_mode(engine, sql, None)
}

/// Returns `true` if any SQL statement in `sql` sets a credential using an optional live
/// MySQL [`MysqlQuoteMode`].
pub fn sql_sets_credentials_with_mysql_mode(
    engine: DbEngine,
    sql: &str,
    mysql_mode: Option<MysqlQuoteMode>,
) -> bool {
    sql_sets_credentials_with_session_mode(engine, sql, SqlSessionMode::mysql(mysql_mode))
}

/// Returns `true` if any SQL statement in `sql` sets a credential using live [`SqlSessionMode`]
/// flags and inline PostgreSQL `standard_conforming_strings` tracking.
pub fn sql_sets_credentials_with_session_mode(
    engine: DbEngine,
    sql: &str,
    session_mode: SqlSessionMode,
) -> bool {
    let mut budget = CredEvalBudget::new();
    sql_sets_credentials_with_session_mode_depth(
        engine,
        sql,
        session_mode,
        false,
        0,
        &mut budget,
    )
}

fn sql_sets_credentials_with_session_mode_depth(
    engine: DbEngine,
    sql: &str,
    session_mode: SqlSessionMode,
    allow_placeholder: bool,
    depth: usize,
    budget: &mut CredEvalBudget,
) -> bool {
    let mysql_mode = session_mode.mysql;
    let stmts = split_statements_for_engine_with_session_mode(sql, engine, session_mode.clone());
    let check_all_modes = mysql_mode.is_none() || mysql_stmt_may_touch_sql_mode_text(sql);
    let outer_mode_known = mysql_mode.is_some();
    let check_tokens = |text: &str, pg_scs: bool, budget: &mut CredEvalBudget| {
        let mut has_credentials = |mode: MysqlQuoteMode| {
            if engine != DbEngine::Mysql {
                let toks = tokenize_statement_for_credentials_with_mode(
                    engine,
                    text,
                    mode,
                    MysqlCommentPolicy::UnconditionalOnly,
                    pg_scs,
                );
                return toks.split(|tok| matches!(tok, CredTok::Punct(';'))).any(|sub| {
                    statement_sets_credentials(
                        engine,
                        sub,
                        mode,
                        outer_mode_known,
                        allow_placeholder,
                        depth,
                        budget,
                    )
                });
            }
            let plausible = plausible_mysql_comment_policies(text, mode, mode.server_version);
            if mode.server_version.is_none() && plausible.budget_exhausted {
                return true;
            }
            plausible.policies.into_iter().any(|policy| {
                let effective_mode = policy.apply_to_mode(mode);
                let toks = tokenize_statement_for_credentials_with_mode(
                    engine,
                    text,
                    effective_mode,
                    policy,
                    true,
                );
                toks.split(|tok| matches!(tok, CredTok::Punct(';'))).any(|sub| {
                    statement_sets_credentials(
                        engine,
                        sub,
                        effective_mode,
                        outer_mode_known,
                        allow_placeholder,
                        depth,
                        budget,
                    )
                })
            })
        };
        if engine != DbEngine::Mysql {
            return has_credentials(MysqlQuoteMode::DEFAULT);
        }
        if let Some(mode) = mysql_mode
            && !check_all_modes
        {
            return has_credentials(mode);
        }
        if let Some(mode) = mysql_mode
            && has_credentials(mode)
        {
            return true;
        }
        if has_credentials(MysqlQuoteMode::DEFAULT) {
            return true;
        }
        let has_esc_dq = text.contains("\\\"");
        let has_esc_sq = text.contains("\\'");
        let has_brackets = text.contains('[');
        (has_esc_dq && has_credentials(MysqlQuoteMode::ANSI_QUOTES))
            || (has_esc_sq && has_credentials(MysqlQuoteMode::NO_BACKSLASH_ESCAPES))
            || (has_esc_dq
                && has_esc_sq
                && has_credentials(MysqlQuoteMode::ANSI_AND_NO_BACKSLASH))
            || (has_brackets && has_credentials(MysqlQuoteMode::MSSQL))
            || (has_brackets
                && has_esc_sq
                && has_credentials(MysqlQuoteMode::MSSQL_AND_NO_BACKSLASH))
    };
    let check_with_unicode_trim = |text: &str, pg_scs: bool, budget: &mut CredEvalBudget| {
        check_tokens(text, pg_scs, budget) || {
            let trimmed = text.trim();
            trimmed != text && check_tokens(trimmed, pg_scs, budget)
        }
    };
    if stmts.is_empty() {
        return check_with_unicode_trim(sql, session_mode.pg_standard_conforming_strings(), budget);
    }
    let mut pg_scs = PgScsTracker::from_mode(&session_mode.pg_session_mode());
    for stmt in &stmts {
        let is_cred = if engine == DbEngine::Postgres && pg_scs.active.is_none() {
            check_with_unicode_trim(&stmt.sql, true, budget)
                || check_with_unicode_trim(&stmt.sql, false, budget)
        } else {
            check_with_unicode_trim(&stmt.sql, pg_scs.active.unwrap_or(true), budget)
        };
        if is_cred {
            return true;
        }
        if engine == DbEngine::Postgres {
            pg_scs.apply(&stmt.sql);
        }
    }
    if engine == DbEngine::Postgres && pg_scs.saw_uncertain && sql.contains("\\'") {
        return check_with_unicode_trim(sql, true, budget)
            || check_with_unicode_trim(sql, false, budget);
    }
    engine == DbEngine::Mysql
        && check_all_modes
        && (sql.contains("\\'") || sql.contains("\\\"") || sql.contains('['))
        && check_with_unicode_trim(sql, true, budget)
}

/// Returns `true` if any statement or combined batch text sets credentials while tracking
/// inline MySQL `SET SESSION sql_mode` transitions from `initial_mysql_mode`.
pub fn batch_sets_credentials_with_mysql_mode(
    engine: DbEngine,
    statements: &[String],
    executed_sql_text: &str,
    initial_mysql_mode: Option<MysqlQuoteMode>,
) -> bool {
    batch_sets_credentials_with_session_mode(
        engine,
        statements,
        executed_sql_text,
        SqlSessionMode::mysql(initial_mysql_mode),
    )
}

/// Returns `true` if any statement or combined batch text sets credentials while tracking
/// inline MySQL `SET SESSION sql_mode` and PostgreSQL `standard_conforming_strings` transitions.
pub fn batch_sets_credentials_with_session_mode(
    engine: DbEngine,
    statements: &[String],
    executed_sql_text: &str,
    initial_mode: SqlSessionMode,
) -> bool {
    let mut budget = CredEvalBudget::new();
    let mut active_mysql = initial_mode.mysql;
    let initial_pg = initial_mode.pg_session_mode();
    let mut pg_scs = PgScsTracker::from_mode(&initial_pg);
    let mut saw_mysql_mode_change = false;
    for stmt in statements {
        let is_cred = if engine == DbEngine::Postgres && pg_scs.active.is_none() {
            sql_sets_credentials_with_session_mode_depth(
                engine,
                stmt,
                SqlSessionMode::postgres(true),
                false,
                0,
                &mut budget,
            ) || sql_sets_credentials_with_session_mode_depth(
                engine,
                stmt,
                SqlSessionMode::postgres(false),
                false,
                0,
                &mut budget,
            )
        } else {
            let active_session = SqlSessionMode {
                mysql: active_mysql,
                postgres: (engine == DbEngine::Postgres).then(|| pg_scs.to_session_mode()),
            };
            sql_sets_credentials_with_session_mode_depth(
                engine,
                stmt,
                active_session,
                false,
                0,
                &mut budget,
            )
        };
        if is_cred {
            return true;
        }
        if engine == DbEngine::Mysql
            && let Some(cur) = active_mysql
        {
            match parse_inline_mysql_set_sql_mode(stmt, cur, true) {
                Some(Some(next)) => {
                    if !next.quote_flags_eq(cur) {
                        saw_mysql_mode_change = true;
                    }
                    active_mysql = Some(next);
                }
                Some(None) => {
                    saw_mysql_mode_change = true;
                    active_mysql = None;
                }
                None => {}
            }
        } else if engine == DbEngine::Postgres {
            pg_scs.apply(stmt);
        }
    }
    if trim_sql_whitespace_for_engine(engine, executed_sql_text).is_empty() {
        return false;
    }
    let split_exec = split_statements_for_engine_with_session_mode(
        executed_sql_text,
        engine,
        initial_mode.clone(),
    );
    let stmts_match = !statements.is_empty()
        && split_exec.len() == statements.len()
        && split_exec.iter().zip(statements).all(|(a, b)| a.sql == *b);
    let pg_needs_recheck = engine == DbEngine::Postgres
        && pg_scs.saw_uncertain
        && executed_sql_text.contains("\\'");
    let needs_buffer_recheck = if !stmts_match || pg_needs_recheck {
        true
    } else if engine != DbEngine::Mysql {
        false
    } else if saw_mysql_mode_change || mysql_stmt_may_touch_sql_mode_text(executed_sql_text) {
        true
    } else {
        initial_mode.mysql.is_none()
            && (executed_sql_text.contains("\\'")
                || executed_sql_text.contains("\\\"")
                || executed_sql_text.contains('['))
    };
    if !needs_buffer_recheck {
        return false;
    }
    if pg_needs_recheck {
        let mut budget_on = CredEvalBudget::new();
        let mut budget_off = CredEvalBudget::new();
        return sql_sets_credentials_with_session_mode_depth(
            engine,
            executed_sql_text,
            SqlSessionMode::postgres(true),
            false,
            0,
            &mut budget_on,
        ) || sql_sets_credentials_with_session_mode_depth(
            engine,
            executed_sql_text,
            SqlSessionMode::postgres(false),
            false,
            0,
            &mut budget_off,
        );
    }
    let buffer_mode = SqlSessionMode {
        mysql: if saw_mysql_mode_change || mysql_stmt_may_touch_sql_mode_text(executed_sql_text) {
            None
        } else {
            initial_mode.mysql
        },
        postgres: initial_mode.postgres,
    };
    let mut buffer_budget = CredEvalBudget::new();
    sql_sets_credentials_with_session_mode_depth(
        engine,
        executed_sql_text,
        buffer_mode,
        false,
        0,
        &mut buffer_budget,
    )
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
