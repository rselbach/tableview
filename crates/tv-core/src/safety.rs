use sqlparser::ast::{
    Query, Set, SetExpr, Statement, TransactionAccessMode, TransactionMode, UtilityOption, Value,
};
use sqlparser::dialect::{MySqlDialect, PostgreSqlDialect, SQLiteDialect};
use sqlparser::parser::Parser;

use crate::connection::{DbEngine, EnvTag, SafetyPolicy};

/// Risk classification for a parsed (or fail-closed unparsed) SQL statement.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StatementRisk {
    ReadOnly,
    WriteDml,
    WriteDdl,
    Destructive,
}

impl StatementRisk {
    pub const fn badge_label(self) -> &'static str {
        match self {
            Self::ReadOnly => "READ",
            Self::WriteDml => "DML",
            Self::WriteDdl => "DDL",
            Self::Destructive => "DESTRUCTIVE",
        }
    }
}

/// Result of inspecting a single SQL statement before execution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatementInspection {
    pub sql: String,
    pub risk: StatementRisk,
    pub summary: String,
    pub parsed: bool,
}

/// Safety evaluation decision for a batch of SQL statements on a connection profile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SafetyDecision {
    Allow,
    RequireConfirmation {
        policy: SafetyPolicy,
        env: EnvTag,
        reason: String,
        statements: Vec<StatementInspection>,
        is_destructive: bool,
    },
    Block {
        policy: SafetyPolicy,
        env: EnvTag,
        reason: String,
        statements: Vec<StatementInspection>,
    },
}

/// Evaluates whether committing staged table changes is permitted under `policy`.
pub fn evaluate_staged_commit_safety(policy: SafetyPolicy) -> Result<(), String> {
    if policy == SafetyPolicy::ReadOnly {
        Err("Read-Only connection blocks committing staged changes".to_owned())
    } else {
        Ok(())
    }
}

/// Inspects a single PostgreSQL statement string via `sqlparser` (`PostgreSqlDialect`).
pub fn inspect_statement(sql: &str) -> StatementInspection {
    inspect_statement_for_engine(DbEngine::Postgres, sql)
}

fn trim_sql_whitespace(engine: DbEngine, s: &str) -> &str {
    crate::sql::trim_sql_whitespace_for_engine(engine, s)
}

/// Inspects a single SQL statement string using the dialect matching `engine`.
pub fn inspect_statement_for_engine(engine: DbEngine, sql: &str) -> StatementInspection {
    inspect_statement_for_engine_with_mysql_mode(engine, sql, None)
}

/// Inspects a single SQL statement string using `engine` and an optional live MySQL [`crate::sql::MysqlQuoteMode`].
pub fn inspect_statement_for_engine_with_mysql_mode(
    engine: DbEngine,
    sql: &str,
    mysql_mode: Option<crate::sql::MysqlQuoteMode>,
) -> StatementInspection {
    inspect_statement_for_engine_with_session_mode(
        engine,
        sql,
        crate::sql::SqlSessionMode::mysql(mysql_mode),
    )
}

/// Inspects a single SQL statement string using `engine` and live [`crate::sql::SqlSessionMode`].
pub fn inspect_statement_for_engine_with_session_mode(
    engine: DbEngine,
    sql: &str,
    session_mode: crate::sql::SqlSessionMode,
) -> StatementInspection {
    let trimmed = trim_sql_whitespace(engine, sql.strip_prefix('\u{feff}').unwrap_or(sql));
    if trimmed.is_empty() {
        return StatementInspection {
            sql: String::new(),
            risk: StatementRisk::ReadOnly,
            summary: "Empty statement".to_owned(),
            parsed: true,
        };
    }

    if engine == DbEngine::Mysql {
        if let Some(mode) = session_mode.mysql {
            return inspect_mysql_with_exact_mode(trimmed, mode);
        }
        return inspect_mysql_statement(trimmed);
    }

    if engine == DbEngine::Postgres && !session_mode.pg_standard_conforming_strings() {
        let normalized = crate::sql::pg_normalize_legacy_escape_strings(trimmed);
        return inspect_single_candidate(
            engine,
            trimmed,
            normalized.as_ref(),
            crate::sql::MysqlQuoteMode::DEFAULT,
        );
    }

    inspect_single_candidate(engine, trimmed, trimmed, crate::sql::MysqlQuoteMode::DEFAULT)
}

fn higher_risk_inspection(
    a: StatementInspection,
    b: StatementInspection,
) -> StatementInspection {
    if b.risk > a.risk || (b.risk == a.risk && b.parsed && !a.parsed) {
        b
    } else {
        a
    }
}

fn fold_higher_risk(
    best: Option<StatementInspection>,
    candidate: StatementInspection,
) -> Option<StatementInspection> {
    Some(match best {
        Some(prev) => higher_risk_inspection(prev, candidate),
        None => candidate,
    })
}

fn inspect_mysql_complete_prefix_statements(
    original_trimmed: &str,
    unwrapped: &str,
    mode: crate::sql::MysqlQuoteMode,
) -> Option<StatementInspection> {
    if !crate::sql::unwrapped_sql_has_multiple_statements(DbEngine::Mysql, unwrapped, mode) {
        return None;
    }
    let parts = crate::sql::split_unwrapped_top_level_statements(DbEngine::Mysql, unwrapped, mode);
    if parts.len() <= 1 {
        return None;
    }
    let mut best: Option<StatementInspection> = None;
    for part in &parts[..parts.len() - 1] {
        let normalized = crate::sql::mysql_normalize_for_parser(part, mode);
        let norm_trimmed = trim_sql_whitespace(DbEngine::Mysql, &normalized);
        if !crate::sql::has_executable_sql_for_engine(norm_trimmed, DbEngine::Mysql) {
            continue;
        }
        let insp = inspect_mysql_normalized_candidate(original_trimmed, norm_trimmed, mode);
        best = fold_higher_risk(best, insp);
    }
    best
}

fn inspect_mysql_with_exact_mode(
    trimmed: &str,
    mode: crate::sql::MysqlQuoteMode,
) -> StatementInspection {
    use crate::sql::{
        mysql_normalize_for_parser, mysql_policies_for_known_server,
        plausible_mysql_comment_policies, unwrap_mysql_executable_comments_with_policy,
    };

    if let Some(sv) = mode.server_version {
        let policies = mysql_policies_for_known_server(trimmed, sv);
        let mut best: Option<StatementInspection> = None;
        for policy in policies {
            let effective_mode = policy.apply_to_mode(mode);
            let (unwrapped, ended_in_quote, _) =
                unwrap_mysql_executable_comments_with_policy(trimmed, effective_mode, policy);
            let insp = if ended_in_quote {
                let prefix_insp =
                    inspect_mysql_complete_prefix_statements(trimmed, &unwrapped, effective_mode);
                let normalized = mysql_normalize_for_parser(&unwrapped, effective_mode);
                let norm_trimmed = trim_sql_whitespace(DbEngine::Mysql, &normalized);
                if norm_trimmed.is_empty() {
                    prefix_insp.unwrap_or_else(|| StatementInspection {
                        sql: trimmed.to_owned(),
                        risk: StatementRisk::ReadOnly,
                        summary: "Empty statement".to_owned(),
                        parsed: true,
                    })
                } else {
                    let single = inspect_single_candidate(
                        DbEngine::Mysql,
                        trimmed,
                        norm_trimmed,
                        effective_mode,
                    );
                    if let Some(prefix) = prefix_insp {
                        higher_risk_inspection(single, prefix)
                    } else {
                        single
                    }
                }
            } else {
                let normalized = mysql_normalize_for_parser(&unwrapped, effective_mode);
                let norm_trimmed = trim_sql_whitespace(DbEngine::Mysql, &normalized);
                inspect_mysql_normalized_candidate(trimmed, norm_trimmed, effective_mode)
            };
            best = fold_higher_risk(best, insp);
        }
        return best.unwrap_or_else(|| StatementInspection {
            sql: trimmed.to_owned(),
            risk: StatementRisk::ReadOnly,
            summary: "Empty statement".to_owned(),
            parsed: true,
        });
    }

    let mut fallback_unwrapped: Option<String> = None;
    let mut best_parsed: Option<StatementInspection> = None;
    let mut best_unparsed: Option<StatementInspection> = None;

    let plausible = plausible_mysql_comment_policies(trimmed, mode, None);
    for policy in plausible.policies {
        let effective_mode = policy.apply_to_mode(mode);
        let (unwrapped, ended_in_quote, _) =
            unwrap_mysql_executable_comments_with_policy(trimmed, effective_mode, policy);
        if ended_in_quote {
            if let Some(prefix) =
                inspect_mysql_complete_prefix_statements(trimmed, &unwrapped, effective_mode)
            {
                if prefix.parsed {
                    if best_parsed.as_ref().is_none_or(|b| prefix.risk > b.risk) {
                        best_parsed = Some(prefix);
                    }
                } else if best_unparsed.as_ref().is_none_or(|b| prefix.risk > b.risk) {
                    best_unparsed = Some(prefix);
                }
            }
            let normalized = mysql_normalize_for_parser(&unwrapped, effective_mode);
            let norm_trimmed = trim_sql_whitespace(DbEngine::Mysql, &normalized);
            if !norm_trimmed.is_empty() {
                let insp =
                    inspect_single_candidate(DbEngine::Mysql, trimmed, norm_trimmed, effective_mode);
                if best_unparsed.as_ref().is_none_or(|b| insp.risk > b.risk) {
                    best_unparsed = Some(insp);
                }
            }
            continue;
        }
        if fallback_unwrapped.is_none() {
            fallback_unwrapped = Some(unwrapped.clone());
        }
        let normalized = mysql_normalize_for_parser(&unwrapped, effective_mode);
        let norm_trimmed = trim_sql_whitespace(DbEngine::Mysql, &normalized);
        if !crate::sql::has_executable_sql_for_engine(norm_trimmed, DbEngine::Mysql) {
            continue;
        }
        let insp = inspect_mysql_normalized_candidate(trimmed, norm_trimmed, effective_mode);
        if insp.parsed {
            if best_parsed.as_ref().is_none_or(|b| insp.risk > b.risk) {
                best_parsed = Some(insp);
            }
        } else if best_unparsed.as_ref().is_none_or(|b| insp.risk > b.risk) {
            best_unparsed = Some(insp);
        }
    }

    if plausible.budget_exhausted
        && best_parsed.as_ref().is_none_or(|b| b.risk < StatementRisk::Destructive)
    {
        return StatementInspection {
            sql: trimmed.to_owned(),
            risk: StatementRisk::Destructive,
            summary: "Conditional MySQL statement (too many version gates to verify safely)".to_owned(),
            parsed: false,
        };
    }
    if let Some(best) = best_parsed {
        return best;
    }
    if let Some(best) = best_unparsed {
        return best;
    }
    let fallback = fallback_unwrapped.as_deref().unwrap_or(trimmed);
    let normalized_fallback = mysql_normalize_for_parser(fallback, mode);
    let fallback_trimmed = trim_sql_whitespace(DbEngine::Mysql, &normalized_fallback);
    if fallback_trimmed.is_empty() {
        return StatementInspection {
            sql: trimmed.to_owned(),
            risk: StatementRisk::ReadOnly,
            summary: "Empty statement".to_owned(),
            parsed: true,
        };
    }
    inspect_single_candidate(DbEngine::Mysql, trimmed, fallback_trimmed, mode)
}

fn inspect_mysql_statement(trimmed: &str) -> StatementInspection {
    inspect_mysql_statement_with_server_version(trimmed, None)
}

fn inspect_mysql_statement_with_server_version(
    trimmed: &str,
    known_server_version: Option<crate::sql::MysqlServerVersion>,
) -> StatementInspection {
    use crate::sql::{
        MysqlFlavor, MysqlQuoteMode, mysql_normalize_for_parser, plausible_mysql_comment_policies,
        unwrap_mysql_executable_comments_with_policy,
    };

    if !trimmed.contains("/*!")
        && !trimmed.contains("/*M!")
        && !trimmed.contains('\\')
        && !trimmed.contains('"')
        && !trimmed.contains('[')
        && !trimmed.contains("--")
        && !trimmed.contains('#')
    {
        return inspect_mysql_normalized_candidate(
            trimmed,
            trimmed,
            MysqlQuoteMode::DEFAULT.with_server_version(known_server_version),
        );
    }

    let has_dq = trimmed.contains('"');
    let has_bs = trimmed.contains('\\');
    let has_br = trimmed.contains('[');
    let mut modes = vec![MysqlQuoteMode::DEFAULT];
    if has_dq {
        modes.push(MysqlQuoteMode::ANSI_QUOTES);
    }
    if has_bs {
        modes.push(MysqlQuoteMode::NO_BACKSLASH_ESCAPES);
    }
    if has_dq && has_bs {
        modes.push(MysqlQuoteMode::ANSI_AND_NO_BACKSLASH);
    }
    if has_br
        && !matches!(
            known_server_version,
            Some(sv) if sv.flavor == MysqlFlavor::Mysql
        )
    {
        modes.push(MysqlQuoteMode::MSSQL);
        if has_bs {
            modes.push(MysqlQuoteMode::MSSQL_AND_NO_BACKSLASH);
        }
    }

    if let Some(sv) = known_server_version {
        let mut best: Option<StatementInspection> = None;
        for base_mode in modes {
            let insp = inspect_mysql_with_exact_mode(trimmed, base_mode.with_server_version(Some(sv)));
            best = fold_higher_risk(best, insp);
        }
        return best.unwrap_or_else(|| StatementInspection {
            sql: trimmed.to_owned(),
            risk: StatementRisk::ReadOnly,
            summary: "Empty statement".to_owned(),
            parsed: true,
        });
    }

    let mut default_unwrapped: Option<String> = None;
    let mut best_parsed: Option<StatementInspection> = None;
    let mut best_unparsed: Option<StatementInspection> = None;
    let mut any_budget_exhausted = false;

    for mode in modes {
        let plausible = plausible_mysql_comment_policies(trimmed, mode, None);
        any_budget_exhausted |= plausible.budget_exhausted;
        for &policy in &plausible.policies {
            let effective_mode = policy.apply_to_mode(mode);
            let (unwrapped, ended_in_quote, _) =
                unwrap_mysql_executable_comments_with_policy(trimmed, effective_mode, policy);
            if mode.quote_flags_eq(MysqlQuoteMode::DEFAULT) && default_unwrapped.is_none() {
                default_unwrapped = Some(unwrapped.clone());
            }
            if ended_in_quote {
                if let Some(prefix) =
                    inspect_mysql_complete_prefix_statements(trimmed, &unwrapped, effective_mode)
                {
                    if prefix.parsed {
                        if best_parsed.as_ref().is_none_or(|b| prefix.risk > b.risk) {
                            best_parsed = Some(prefix);
                        }
                    } else if best_unparsed.as_ref().is_none_or(|b| prefix.risk > b.risk) {
                        best_unparsed = Some(prefix);
                    }
                }
                continue;
            }
            let normalized = mysql_normalize_for_parser(&unwrapped, effective_mode);
            let norm_trimmed = trim_sql_whitespace(DbEngine::Mysql, &normalized);
            if !crate::sql::has_executable_sql_for_engine(norm_trimmed, DbEngine::Mysql) {
                continue;
            }
            let insp = inspect_mysql_normalized_candidate(trimmed, norm_trimmed, effective_mode);
            if insp.parsed {
                if best_parsed.as_ref().is_none_or(|b| insp.risk > b.risk) {
                    best_parsed = Some(insp);
                }
            } else if best_unparsed.as_ref().is_none_or(|b| insp.risk > b.risk) {
                best_unparsed = Some(insp);
            }
        }
    }

    if any_budget_exhausted
        && best_parsed.as_ref().is_none_or(|b| b.risk < StatementRisk::Destructive)
    {
        return StatementInspection {
            sql: trimmed.to_owned(),
            risk: StatementRisk::Destructive,
            summary: "Conditional MySQL statement (too many version gates to verify safely)".to_owned(),
            parsed: false,
        };
    }
    if let Some(best) = best_parsed {
        return best;
    }
    if let Some(best) = best_unparsed {
        return best;
    }
    let fallback = default_unwrapped.as_deref().unwrap_or(trimmed);
    let normalized_fallback = mysql_normalize_for_parser(fallback, MysqlQuoteMode::DEFAULT);
    let fallback_trimmed = trim_sql_whitespace(DbEngine::Mysql, &normalized_fallback);
    if fallback_trimmed.is_empty() {
        return StatementInspection {
            sql: trimmed.to_owned(),
            risk: StatementRisk::ReadOnly,
            summary: "Empty statement".to_owned(),
            parsed: true,
        };
    }
    inspect_single_candidate(DbEngine::Mysql, trimmed, fallback_trimmed, MysqlQuoteMode::DEFAULT)
}

const MAX_COMPOUND_INSPECT_DEPTH: usize = 8;

fn inspect_mysql_normalized_candidate(
    original_trimmed: &str,
    norm_trimmed: &str,
    mode: crate::sql::MysqlQuoteMode,
) -> StatementInspection {
    inspect_mysql_normalized_candidate_depth(original_trimmed, norm_trimmed, mode, 0)
}

fn inspect_mysql_normalized_candidate_depth(
    original_trimmed: &str,
    norm_trimmed: &str,
    mode: crate::sql::MysqlQuoteMode,
    depth: usize,
) -> StatementInspection {
    if norm_trimmed.is_empty() {
        return StatementInspection {
            sql: original_trimmed.to_owned(),
            risk: StatementRisk::ReadOnly,
            summary: "Empty statement".to_owned(),
            parsed: true,
        };
    }
    let insp = inspect_single_candidate(DbEngine::Mysql, original_trimmed, norm_trimmed, mode);
    if !insp.parsed && mysql_valid_select_into_outfile_or_dumpfile(norm_trimmed, mode) {
        return StatementInspection {
            sql: original_trimmed.to_owned(),
            risk: StatementRisk::WriteDml,
            summary: "SELECT INTO OUTFILE / DUMPFILE".to_owned(),
            parsed: true,
        };
    }
    if !insp.parsed
        && let Some(wrapped) =
            inspect_mysql_set_statement_inner(original_trimmed, norm_trimmed, mode, depth)
    {
        return wrapped;
    }
    if !insp.parsed
        && let Some(compound) =
            inspect_mysql_compound_block_inner(original_trimmed, norm_trimmed, mode, depth)
    {
        return compound;
    }
    insp
}

fn inspect_mysql_set_statement_inner(
    original_trimmed: &str,
    part: &str,
    mode: crate::sql::MysqlQuoteMode,
    depth: usize,
) -> Option<StatementInspection> {
    if depth >= MAX_COMPOUND_INSPECT_DEPTH {
        return None;
    }
    let inner = crate::sql::strip_all_mariadb_set_statement_prefixes(part, mode)?;
    let mut insp = inspect_mysql_normalized_candidate_depth(original_trimmed, inner, mode, depth + 1);
    if insp.risk < StatementRisk::WriteDml {
        insp.risk = StatementRisk::WriteDml;
    }
    Some(insp)
}

fn inspect_mysql_compound_block_inner(
    original_trimmed: &str,
    part: &str,
    mode: crate::sql::MysqlQuoteMode,
    depth: usize,
) -> Option<StatementInspection> {
    if depth >= MAX_COMPOUND_INSPECT_DEPTH {
        return None;
    }
    let inner_stmts = crate::sql::extract_mariadb_compound_inner_statements(part, mode)?;
    let mut best = StatementInspection {
        sql: original_trimmed.to_owned(),
        risk: StatementRisk::WriteDml,
        summary: "Compound SQL block".to_owned(),
        parsed: false,
    };
    for inner in inner_stmts {
        let insp = inspect_single_leaf_candidate_depth(
            DbEngine::Mysql,
            original_trimmed,
            &inner,
            mode,
            depth + 1,
        );
        if insp.risk > best.risk || (insp.risk == best.risk && insp.parsed && !best.parsed) {
            best = insp;
        }
    }
    if best.risk < StatementRisk::WriteDml {
        best.risk = StatementRisk::WriteDml;
    }
    Some(best)
}

fn mysql_valid_select_into_outfile_or_dumpfile(
    sql: &str,
    mode: crate::sql::MysqlQuoteMode,
) -> bool {
    let bytes = sql.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;
    let mut paren_depth = 0usize;
    let mut in_sq = false;
    let mut in_dq = false;
    let mut in_bt = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;

    while i < len {
        if in_line_comment {
            if bytes[i] == b'\n' {
                in_line_comment = false;
            }
            i += 1;
            continue;
        }
        if in_block_comment {
            if i + 1 < len && bytes[i] == b'*' && bytes[i + 1] == b'/' {
                in_block_comment = false;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if in_sq {
            if bytes[i] == b'\\' {
                i += 2.min(len - i);
            } else if bytes[i] == b'\'' {
                in_sq = false;
                i += 1;
            } else {
                i += 1;
            }
            continue;
        }
        if in_dq {
            if bytes[i] == b'\\' {
                i += 2.min(len - i);
            } else if bytes[i] == b'"' {
                in_dq = false;
                i += 1;
            } else {
                i += 1;
            }
            continue;
        }
        if in_bt {
            if bytes[i] == b'`' {
                if i + 1 < len && bytes[i + 1] == b'`' {
                    i += 2;
                } else {
                    in_bt = false;
                    i += 1;
                }
            } else {
                i += 1;
            }
            continue;
        }

        if bytes[i] == b'\'' {
            in_sq = true;
            i += 1;
        } else if bytes[i] == b'"' {
            in_dq = true;
            i += 1;
        } else if bytes[i] == b'`' {
            in_bt = true;
            i += 1;
        } else if crate::sql::is_dash_comment_start(DbEngine::Mysql, sql, i) {
            in_line_comment = true;
            i += 2;
        } else if bytes[i] == b'#' {
            in_line_comment = true;
            i += 1;
        } else if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            in_block_comment = true;
            i += 2;
        } else if bytes[i] == b'(' {
            paren_depth += 1;
            i += 1;
        } else if bytes[i] == b')' {
            paren_depth = paren_depth.saturating_sub(1);
            i += 1;
        } else if paren_depth == 0 && (bytes[i].is_ascii_alphabetic() || bytes[i] == b'_') {
            let word_start = i;
            i += 1;
            while i < len && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'$') {
                i += 1;
            }
            if sql[word_start..i].eq_ignore_ascii_case("INTO") {
                let mut k = skip_mysql_ws_and_comments(sql, i);
                let target_start = k;
                while k < len && (bytes[k].is_ascii_alphanumeric() || bytes[k] == b'_') {
                    k += 1;
                }
                let kw = &sql[target_start..k];
                if kw.eq_ignore_ascii_case("OUTFILE") || kw.eq_ignore_ascii_case("DUMPFILE") {
                    let lit = skip_mysql_ws_and_comments(sql, k);
                    if lit < len
                        && (bytes[lit] == b'\'' || (!mode.ansi_quotes && bytes[lit] == b'"'))
                        && crate::sql::is_closed_mysql_string_literal(sql, lit)
                    {
                        let prefix = trim_sql_whitespace(DbEngine::Mysql, &sql[..word_start]);
                        if !prefix.is_empty()
                            && Parser::parse_sql(&MySqlDialect {}, prefix)
                                .is_ok_and(|stmts| matches!(stmts.as_slice(), [Statement::Query(_)]))
                        {
                            return true;
                        }
                    }
                }
            }
        } else {
            let ch = sql[i..].chars().next().unwrap_or('\0');
            i += ch.len_utf8();
        }
    }
    false
}

fn skip_mysql_ws_and_comments(sql: &str, mut i: usize) -> usize {
    let bytes = sql.as_bytes();
    let len = bytes.len();
    while i < len {
        let ch = sql[i..].chars().next().unwrap_or('\0');
        if crate::sql::is_sql_whitespace(DbEngine::Mysql, ch) {
            i += ch.len_utf8();
        } else if crate::sql::is_dash_comment_start(DbEngine::Mysql, sql, i) {
            i += 2;
            while i < len && bytes[i] != b'\n' {
                i += 1;
            }
        } else if bytes[i] == b'#' {
            i += 1;
            while i < len && bytes[i] != b'\n' {
                i += 1;
            }
        } else if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            while i < len {
                if i + 1 < len && bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    i += 2;
                    break;
                }
                i += 1;
            }
        } else {
            break;
        }
    }
    i
}

fn inspect_single_candidate(
    engine: DbEngine,
    original_trimmed: &str,
    parse_trimmed: &str,
    mysql_mode: crate::sql::MysqlQuoteMode,
) -> StatementInspection {
    let parsed = match engine {
        DbEngine::Postgres => Parser::parse_sql(&PostgreSqlDialect {}, parse_trimmed),
        DbEngine::Mysql => Parser::parse_sql(&MySqlDialect {}, parse_trimmed),
        DbEngine::Sqlite => Parser::parse_sql(&SQLiteDialect {}, parse_trimmed),
    };

    match parsed {
        Ok(stmts) if !stmts.is_empty() => {
            let mut max_risk = StatementRisk::ReadOnly;
            let mut best_summary = "SELECT".to_owned();
            for stmt in &stmts {
                let (risk, summary) = classify_ast_statement(engine, stmt, parse_trimmed);
                if risk > max_risk || max_risk == StatementRisk::ReadOnly {
                    max_risk = risk;
                    best_summary = summary;
                }
            }
            StatementInspection {
                sql: original_trimmed.to_owned(),
                risk: max_risk,
                summary: best_summary,
                parsed: true,
            }
        }
        Ok(_) => {
            if crate::sql::has_executable_sql_for_engine(parse_trimmed, engine) {
                inspect_unparsed_or_split_candidate(engine, original_trimmed, parse_trimmed, mysql_mode)
            } else {
                StatementInspection {
                    sql: original_trimmed.to_owned(),
                    risk: StatementRisk::ReadOnly,
                    summary: "Empty statement".to_owned(),
                    parsed: true,
                }
            }
        }
        Err(_) => inspect_unparsed_or_split_candidate(engine, original_trimmed, parse_trimmed, mysql_mode),
    }
}

fn inspect_single_leaf_candidate(
    engine: DbEngine,
    original_trimmed: &str,
    part: &str,
    mysql_mode: crate::sql::MysqlQuoteMode,
) -> StatementInspection {
    inspect_single_leaf_candidate_depth(engine, original_trimmed, part, mysql_mode, 0)
}

fn inspect_single_leaf_candidate_depth(
    engine: DbEngine,
    original_trimmed: &str,
    part: &str,
    mysql_mode: crate::sql::MysqlQuoteMode,
    depth: usize,
) -> StatementInspection {
    let parsed = match engine {
        DbEngine::Postgres => Parser::parse_sql(&PostgreSqlDialect {}, part),
        DbEngine::Mysql => Parser::parse_sql(&MySqlDialect {}, part),
        DbEngine::Sqlite => Parser::parse_sql(&SQLiteDialect {}, part),
    };

    match parsed {
        Ok(stmts) if !stmts.is_empty() => {
            let mut max_risk = StatementRisk::ReadOnly;
            let mut best_summary = "SELECT".to_owned();
            for stmt in &stmts {
                let (risk, summary) = classify_ast_statement(engine, stmt, part);
                if risk > max_risk || max_risk == StatementRisk::ReadOnly {
                    max_risk = risk;
                    best_summary = summary;
                }
            }
            StatementInspection {
                sql: original_trimmed.to_owned(),
                risk: max_risk,
                summary: best_summary,
                parsed: true,
            }
        }
        _ => {
            if engine == DbEngine::Mysql
                && let Some(wrapped) =
                    inspect_mysql_set_statement_inner(original_trimmed, part, mysql_mode, depth)
            {
                return wrapped;
            }
            if engine == DbEngine::Mysql
                && let Some(compound) =
                    inspect_mysql_compound_block_inner(original_trimmed, part, mysql_mode, depth)
            {
                return compound;
            }
            if engine == DbEngine::Mysql && mysql_valid_select_into_outfile_or_dumpfile(part, mysql_mode) {
                return StatementInspection {
                    sql: original_trimmed.to_owned(),
                    risk: StatementRisk::WriteDml,
                    summary: "SELECT INTO OUTFILE / DUMPFILE".to_owned(),
                    parsed: true,
                };
            }
            let mut insp = classify_unparsed_statement(engine, part, mysql_mode);
            insp.sql = original_trimmed.to_owned();
            insp
        }
    }
}

fn inspect_unparsed_or_split_candidate(
    engine: DbEngine,
    original_trimmed: &str,
    parse_trimmed: &str,
    mysql_mode: crate::sql::MysqlQuoteMode,
) -> StatementInspection {
    let parts = crate::sql::split_unwrapped_top_level_statements(engine, parse_trimmed, mysql_mode);
    if parts.len() > 1 || parts.first().is_some_and(|p| p != parse_trimmed) {
        let mut best: Option<StatementInspection> = None;
        for part in &parts {
            let insp = inspect_single_leaf_candidate(engine, original_trimmed, part, mysql_mode);
            if best
                .as_ref()
                .is_none_or(|b| insp.risk > b.risk || (insp.risk == b.risk && insp.parsed && !b.parsed))
            {
                best = Some(insp);
            }
        }
        if let Some(best) = best {
            return best;
        }
    }
    let mut insp = classify_unparsed_statement(engine, parse_trimmed, mysql_mode);
    insp.sql = original_trimmed.to_owned();
    insp
}

fn classify_unparsed_statement(
    engine: DbEngine,
    trimmed: &str,
    mysql_mode: crate::sql::MysqlQuoteMode,
) -> StatementInspection {
    if crate::sql::unwrapped_sql_has_multiple_statements(engine, trimmed, mysql_mode) {
        return StatementInspection {
            sql: trimmed.to_owned(),
            risk: StatementRisk::WriteDml,
            summary: "Unparsed SQL statement (treated as write for safety)".to_owned(),
            parsed: false,
        };
    }

    let stripped = strip_leading_sql_comments(trimmed, engine);
    let upper = stripped.to_ascii_uppercase();

    if starts_with_keyword(engine, &upper, "SHOW") || starts_with_keyword(engine, &upper, "VALUES") {
        return StatementInspection {
            sql: trimmed.to_owned(),
            risk: StatementRisk::ReadOnly,
            summary: "SHOW / VALUES".to_owned(),
            parsed: false,
        };
    }

    match engine {
        DbEngine::Mysql => {
            if starts_with_keyword(engine, &upper, "DESCRIBE")
                || starts_with_keyword(engine, &upper, "DESC")
            {
                return StatementInspection {
                    sql: trimmed.to_owned(),
                    risk: StatementRisk::ReadOnly,
                    summary: "DESCRIBE".to_owned(),
                    parsed: false,
                };
            }
            if starts_with_keyword(engine, &upper, "USE") {
                return StatementInspection {
                    sql: trimmed.to_owned(),
                    risk: StatementRisk::ReadOnly,
                    summary: "USE".to_owned(),
                    parsed: false,
                };
            }
            if starts_with_keyword(engine, &upper, "REPLACE") {
                return StatementInspection {
                    sql: trimmed.to_owned(),
                    risk: StatementRisk::WriteDml,
                    summary: "REPLACE".to_owned(),
                    parsed: false,
                };
            }
        }
        DbEngine::Sqlite => {
            if starts_with_keyword(engine, &upper, "PRAGMA") {
                if pragma_has_assignment(stripped) {
                    return StatementInspection {
                        sql: trimmed.to_owned(),
                        risk: StatementRisk::WriteDdl,
                        summary: "PRAGMA".to_owned(),
                        parsed: false,
                    };
                }
                return StatementInspection {
                    sql: trimmed.to_owned(),
                    risk: StatementRisk::ReadOnly,
                    summary: "PRAGMA".to_owned(),
                    parsed: false,
                };
            }
            if upper.starts_with("EXPLAIN QUERY PLAN")
                && upper["EXPLAIN QUERY PLAN".len()..]
                    .chars()
                    .next()
                    .is_none_or(|c| crate::sql::is_sql_whitespace(DbEngine::Sqlite, c))
            {
                return StatementInspection {
                    sql: trimmed.to_owned(),
                    risk: StatementRisk::ReadOnly,
                    summary: "EXPLAIN QUERY PLAN".to_owned(),
                    parsed: false,
                };
            }
            if starts_with_keyword(engine, &upper, "ATTACH")
                || starts_with_keyword(engine, &upper, "DETACH")
                || starts_with_keyword(engine, &upper, "REINDEX")
                || starts_with_keyword(engine, &upper, "VACUUM")
            {
                return StatementInspection {
                    sql: trimmed.to_owned(),
                    risk: StatementRisk::WriteDdl,
                    summary: "SQLite DDL / Utility".to_owned(),
                    parsed: false,
                };
            }
        }
        DbEngine::Postgres => {}
    }

    // Fail-closed on unparseable engine-specific statements so guardrails cannot be bypassed.
    StatementInspection {
        sql: trimmed.to_owned(),
        risk: StatementRisk::WriteDml,
        summary: "Unparsed SQL statement (treated as write for safety)".to_owned(),
        parsed: false,
    }
}

fn starts_with_keyword(engine: DbEngine, upper: &str, kw: &str) -> bool {
    upper.starts_with(kw)
        && upper[kw.len()..]
            .chars()
            .next()
            .is_none_or(|c| crate::sql::is_sql_whitespace(engine, c) || c == ';')
}

fn pragma_has_assignment(sql: &str) -> bool {
    // `PRAGMA name = val` sets a pragma; `PRAGMA name` or `PRAGMA name(arg)` reads.
    let mut in_single = false;
    let mut in_double = false;
    let mut paren_depth = 0usize;
    for ch in sql.chars() {
        match ch {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '(' if !in_single && !in_double => paren_depth += 1,
            ')' if !in_single && !in_double => paren_depth = paren_depth.saturating_sub(1),
            '=' if !in_single && !in_double && paren_depth == 0 => return true,
            _ => {}
        }
    }
    false
}

/// Evaluates all SQL statements against the connection's [`SafetyPolicy`] and [`EnvTag`].
pub fn evaluate_sql_safety(policy: SafetyPolicy, env: EnvTag, statements: &[String]) -> SafetyDecision {
    evaluate_sql_safety_for_engine(DbEngine::Postgres, policy, env, statements)
}

/// Evaluates all SQL statements for `engine` against [`SafetyPolicy`] and [`EnvTag`].
pub fn evaluate_sql_safety_for_engine(
    engine: DbEngine,
    policy: SafetyPolicy,
    env: EnvTag,
    statements: &[String],
) -> SafetyDecision {
    evaluate_sql_safety_for_engine_with_mysql_mode(engine, policy, env, statements, None)
}

/// Evaluates all SQL statements for `engine` against [`SafetyPolicy`] and [`EnvTag`], using an
/// optional live MySQL [`crate::sql::MysqlQuoteMode`] until a statement mentions `sql_mode`.
pub fn evaluate_sql_safety_for_engine_with_mysql_mode(
    engine: DbEngine,
    policy: SafetyPolicy,
    env: EnvTag,
    statements: &[String],
    mysql_mode: Option<crate::sql::MysqlQuoteMode>,
) -> SafetyDecision {
    evaluate_sql_safety_for_engine_with_session_mode(
        engine,
        policy,
        env,
        statements,
        crate::sql::SqlSessionMode::mysql(mysql_mode),
    )
}

/// Evaluates all SQL statements for `engine` against [`SafetyPolicy`] and [`EnvTag`], tracking
/// inline MySQL `sql_mode` and PostgreSQL `standard_conforming_strings` transitions across batches.
pub fn evaluate_sql_safety_for_engine_with_session_mode(
    engine: DbEngine,
    policy: SafetyPolicy,
    env: EnvTag,
    statements: &[String],
    session_mode: crate::sql::SqlSessionMode,
) -> SafetyDecision {
    let known_server_version = session_mode.mysql.and_then(|m| m.server_version);
    let mut mutating = Vec::new();
    let mut active_mysql = session_mode.mysql;
    let mut pg_scs = crate::sql::PgScsTracker::from_mode(&session_mode.pg_session_mode());
    for raw in statements {
        let trimmed = trim_sql_whitespace(engine, raw);
        if trimmed.is_empty() {
            continue;
        }
        let insp = if engine == DbEngine::Mysql && active_mysql.is_none() && known_server_version.is_some() {
            inspect_mysql_statement_with_server_version(trimmed, known_server_version)
        } else if engine == DbEngine::Postgres && pg_scs.active.is_none() {
            let on_insp = inspect_statement_for_engine_with_session_mode(
                engine,
                trimmed,
                crate::sql::SqlSessionMode::postgres(true),
            );
            let off_insp = inspect_statement_for_engine_with_session_mode(
                engine,
                trimmed,
                crate::sql::SqlSessionMode::postgres(false),
            );
            higher_risk_inspection(on_insp, off_insp)
        } else {
            inspect_statement_for_engine_with_session_mode(
                engine,
                trimmed,
                crate::sql::SqlSessionMode {
                    mysql: active_mysql,
                    postgres: (engine == DbEngine::Postgres).then(|| pg_scs.to_session_mode()),
                },
            )
        };
        if insp.risk > StatementRisk::ReadOnly {
            mutating.push(insp);
        }
        if engine == DbEngine::Mysql
            && let Some(cur) = active_mysql
        {
            match crate::sql::parse_inline_mysql_set_sql_mode(trimmed, cur, true) {
                Some(Some(next)) => active_mysql = Some(next),
                Some(None) => active_mysql = None,
                None => {}
            }
        } else if engine == DbEngine::Postgres {
            pg_scs.apply(trimmed);
        }
    }

    if mutating.is_empty() {
        return SafetyDecision::Allow;
    }

    let is_destructive = mutating.iter().any(|s| s.risk == StatementRisk::Destructive);

    match policy {
        SafetyPolicy::ReadOnly => SafetyDecision::Block {
            policy,
            env,
            reason: "Read-Only connection blocks mutating SQL statements".to_owned(),
            statements: mutating,
        },
        SafetyPolicy::StagedOnly => SafetyDecision::Block {
            policy,
            env,
            reason: "Staged Diff Only connection blocks direct mutating SQL; edit rows in the Data Grid and commit via Staged Diff review".to_owned(),
            statements: mutating,
        },
        SafetyPolicy::ConfirmWrites => SafetyDecision::RequireConfirmation {
            policy,
            env,
            reason: if is_destructive {
                "Confirm Writes policy requires approval before running destructive SQL".to_owned()
            } else {
                "Confirm Writes policy requires approval before running mutating SQL".to_owned()
            },
            statements: mutating,
            is_destructive,
        },
        SafetyPolicy::Unrestricted => {
            if env == EnvTag::Production && is_destructive {
                SafetyDecision::RequireConfirmation {
                    policy,
                    env,
                    reason: "Production safeguard: destructive SQL requires explicit confirmation".to_owned(),
                    statements: mutating,
                    is_destructive: true,
                }
            } else {
                SafetyDecision::Allow
            }
        }
    }
}

fn classify_ast_statement(engine: DbEngine, stmt: &Statement, raw_sql: &str) -> (StatementRisk, String) {
    match stmt {
        Statement::Query(query) => classify_query(engine, query, raw_sql),
        Statement::Explain {
            analyze,
            statement,
            options,
            query_plan,
            ..
        } => {
            if engine == DbEngine::Sqlite || *query_plan {
                return (
                    StatementRisk::ReadOnly,
                    if *query_plan { "EXPLAIN QUERY PLAN" } else { "EXPLAIN" }.to_owned(),
                );
            }
            let runs_query = *analyze
                || options
                    .as_ref()
                    .is_some_and(|opts| opts.iter().any(is_truthy_analyze_option));
            if runs_query {
                let (inner_risk, inner_summary) = classify_ast_statement(engine, statement, raw_sql);
                if inner_risk == StatementRisk::ReadOnly {
                    (StatementRisk::ReadOnly, format!("EXPLAIN ANALYZE ({inner_summary})"))
                } else {
                    (inner_risk, format!("EXPLAIN ANALYZE ({inner_summary})"))
                }
            } else {
                (StatementRisk::ReadOnly, "EXPLAIN".to_owned())
            }
        }
        Statement::ExplainTable { .. } => (StatementRisk::ReadOnly, "DESCRIBE".to_owned()),
        Statement::Use(_) => (StatementRisk::ReadOnly, "USE".to_owned()),
        Statement::Pragma { is_eq, value, .. } => {
            if *is_eq || (value.is_some() && pragma_has_assignment(raw_sql)) {
                (StatementRisk::WriteDdl, "PRAGMA".to_owned())
            } else {
                (StatementRisk::ReadOnly, "PRAGMA".to_owned())
            }
        }
        Statement::ShowFunctions { .. }
        | Statement::ShowVariable { .. }
        | Statement::ShowStatus { .. }
        | Statement::ShowVariables { .. }
        | Statement::ShowCreate { .. }
        | Statement::ShowColumns { .. }
        | Statement::ShowCatalogs { .. }
        | Statement::ShowDatabases { .. }
        | Statement::ShowProcessList { .. }
        | Statement::ShowSchemas { .. }
        | Statement::ShowCharset(_)
        | Statement::ShowObjects(_)
        | Statement::ShowTables { .. }
        | Statement::ShowViews { .. }
        | Statement::ShowCollation { .. } => (StatementRisk::ReadOnly, "SHOW".to_owned()),
        Statement::Insert(ins) => {
            if ins.replace_into {
                (StatementRisk::WriteDml, "REPLACE".to_owned())
            } else {
                (StatementRisk::WriteDml, "INSERT".to_owned())
            }
        }
        Statement::Merge(_) => (StatementRisk::WriteDml, "MERGE".to_owned()),
        Statement::Update(upd) => {
            if upd.selection.is_none() {
                (StatementRisk::Destructive, "UPDATE without WHERE clause".to_owned())
            } else {
                (StatementRisk::WriteDml, "UPDATE".to_owned())
            }
        }
        Statement::Delete(del) => {
            if del.selection.is_none() {
                (StatementRisk::Destructive, "DELETE without WHERE clause".to_owned())
            } else {
                (StatementRisk::WriteDml, "DELETE".to_owned())
            }
        }
        Statement::Copy { to, .. } => {
            if *to {
                (StatementRisk::ReadOnly, "COPY TO".to_owned())
            } else {
                (StatementRisk::WriteDml, "COPY FROM".to_owned())
            }
        }
        Statement::Execute { .. } | Statement::Call(_) => {
            (StatementRisk::WriteDml, "CALL / EXECUTE".to_owned())
        }
        Statement::Truncate(_) => (StatementRisk::Destructive, "TRUNCATE".to_owned()),
        Statement::Drop { .. }
        | Statement::DropFunction(_)
        | Statement::DropDomain(_)
        | Statement::DropProcedure { .. }
        | Statement::DropPolicy(_)
        | Statement::DropRule(_)
        | Statement::DropExtension(_)
        | Statement::DropOperator(_)
        | Statement::DropOperatorFamily(_)
        | Statement::DropOperatorClass(_)
        | Statement::DropTrigger(_) => (StatementRisk::Destructive, "DROP".to_owned()),
        Statement::Set(Set::SetTransaction { modes, .. })
            if !modes.is_empty()
                && modes
                    .iter()
                    .all(|m| matches!(m, TransactionMode::AccessMode(TransactionAccessMode::ReadOnly))) =>
        {
            (StatementRisk::ReadOnly, "SET TRANSACTION READ ONLY".to_owned())
        }
        Statement::StartTransaction { .. } => (StatementRisk::WriteDdl, "BEGIN / START TRANSACTION".to_owned()),
        Statement::Commit { .. } => (StatementRisk::WriteDdl, "COMMIT".to_owned()),
        Statement::Rollback { .. } => (StatementRisk::WriteDdl, "ROLLBACK".to_owned()),
        Statement::Set(_) => (StatementRisk::WriteDdl, "SET".to_owned()),
        Statement::CreateTable(_)
        | Statement::CreateView(_)
        | Statement::CreateIndex(_)
        | Statement::CreateSchema { .. }
        | Statement::CreateDatabase { .. }
        | Statement::CreateFunction(_)
        | Statement::CreateProcedure { .. }
        | Statement::CreateTrigger(_)
        | Statement::CreateType { .. }
        | Statement::CreateDomain(_)
        | Statement::CreateExtension(_)
        | Statement::CreateRole(_)
        | Statement::CreatePolicy(_)
        | Statement::CreateSequence { .. }
        | Statement::AlterTable(_)
        | Statement::AlterIndex { .. }
        | Statement::AlterView { .. }
        | Statement::AlterSchema(_)
        | Statement::AlterRole { .. }
        | Statement::AlterPolicy(_)
        | Statement::AlterFunction(_)
        | Statement::AlterType(_)
        | Statement::Grant { .. }
        | Statement::Revoke { .. }
        | Statement::Comment { .. }
        | Statement::Vacuum(_)
        | Statement::Analyze(_)
        | Statement::AttachDatabase { .. }
        | Statement::AttachDuckDBDatabase { .. }
        | Statement::DetachDuckDBDatabase { .. }
        | Statement::Discard { .. } => (StatementRisk::WriteDdl, "DDL / Utility".to_owned()),
        _ => (StatementRisk::WriteDdl, "SQL Command".to_owned()),
    }
}

fn classify_query(engine: DbEngine, query: &Query, raw_sql: &str) -> (StatementRisk, String) {
    let mut worst_risk = StatementRisk::ReadOnly;
    let mut best_summary = "SELECT".to_owned();

    if let Some(with) = &query.with {
        for cte in &with.cte_tables {
            let (risk, summary) = classify_query(engine, &cte.query, raw_sql);
            if risk > worst_risk {
                worst_risk = risk;
                best_summary = format!("WITH CTE ({summary})");
            }
        }
    }

    let (body_risk, body_summary) = classify_set_expr(engine, &query.body, raw_sql);
    if body_risk > worst_risk {
        worst_risk = body_risk;
        best_summary = body_summary;
    }

    (worst_risk, best_summary)
}

fn classify_set_expr(engine: DbEngine, expr: &SetExpr, raw_sql: &str) -> (StatementRisk, String) {
    match expr {
        SetExpr::Select(sel) => {
            if sel.into.is_some() {
                (StatementRisk::WriteDml, "SELECT INTO".to_owned())
            } else {
                (StatementRisk::ReadOnly, "SELECT".to_owned())
            }
        }
        SetExpr::Query(q) => classify_query(engine, q, raw_sql),
        SetExpr::SetOperation { left, right, .. } => {
            let (lr, ls) = classify_set_expr(engine, left, raw_sql);
            let (rr, rs) = classify_set_expr(engine, right, raw_sql);
            if lr >= rr {
                (lr, ls)
            } else {
                (rr, rs)
            }
        }
        SetExpr::Values(_) | SetExpr::Table(_) => (StatementRisk::ReadOnly, "SELECT".to_owned()),
        SetExpr::Insert(s) | SetExpr::Update(s) | SetExpr::Delete(s) | SetExpr::Merge(s) => {
            classify_ast_statement(engine, s, raw_sql)
        }
    }
}

fn is_truthy_analyze_option(opt: &UtilityOption) -> bool {
    if !opt.name.value.eq_ignore_ascii_case("analyze") {
        return false;
    }
    match &opt.arg {
        None => true,
        Some(sqlparser::ast::Expr::Value(val)) => match &val.value {
            Value::Boolean(b) => *b,
            Value::SingleQuotedString(s) => s.eq_ignore_ascii_case("true") || s == "1",
            _ => true,
        },
        _ => true,
    }
}

fn strip_leading_sql_comments(mut s: &str, engine: DbEngine) -> &str {
    loop {
        s = s.trim_start_matches(|c: char| crate::sql::is_sql_whitespace(engine, c));
        if crate::sql::is_dash_comment_start(engine, s, 0) {
            let rest = &s[2..];
            let end = if engine == DbEngine::Postgres {
                rest.find(['\n', '\r'])
            } else {
                rest.find('\n')
            };
            match end {
                Some(nl) => s = &rest[nl + 1..],
                None => return "",
            }
        } else if engine == DbEngine::Mysql
            && let Some(rest) = s.strip_prefix('#')
        {
            match rest.find('\n') {
                Some(nl) => s = &rest[nl + 1..],
                None => return "",
            }
        } else if let Some(rest) = s.strip_prefix("/*") {
            if engine == DbEngine::Mysql && (s.starts_with("/*!") || s.starts_with("/*M!")) {
                return s;
            }
            if engine == DbEngine::Postgres {
                let bytes = s.as_bytes();
                let mut i = 2usize;
                let mut depth = 1usize;
                while i < bytes.len() && depth > 0 {
                    if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
                        depth += 1;
                        i += 2;
                    } else if i + 1 < bytes.len() && bytes[i] == b'*' && bytes[i + 1] == b'/' {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += s[i..].chars().next().map_or(1, char::len_utf8);
                    }
                }
                if depth > 0 {
                    return "";
                }
                s = &s[i..];
            } else {
                match rest.find("*/") {
                    Some(end) => s = &rest[end + 2..],
                    None => return "",
                }
            }
        } else {
            return s;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inspects_mysql_and_sqlite_statements_accurately() {
        assert_eq!(
            inspect_statement_for_engine(DbEngine::Mysql, "SHOW TABLES").risk,
            StatementRisk::ReadOnly
        );
        assert_eq!(
            inspect_statement_for_engine(DbEngine::Mysql, "DESCRIBE students").risk,
            StatementRisk::ReadOnly
        );
        assert_eq!(
            inspect_statement_for_engine(DbEngine::Mysql, "DESC students").risk,
            StatementRisk::ReadOnly
        );
        assert_eq!(
            inspect_statement_for_engine(DbEngine::Mysql, "USE greendale").risk,
            StatementRisk::ReadOnly
        );
        assert_eq!(
            inspect_statement_for_engine(DbEngine::Mysql, "REPLACE INTO students VALUES (1, 'Troy')").risk,
            StatementRisk::WriteDml
        );

        assert_eq!(
            inspect_statement_for_engine(DbEngine::Sqlite, "PRAGMA table_info('students')").risk,
            StatementRisk::ReadOnly
        );
        assert_eq!(
            inspect_statement_for_engine(DbEngine::Sqlite, "PRAGMA journal_mode").risk,
            StatementRisk::ReadOnly
        );
        assert_eq!(
            inspect_statement_for_engine(DbEngine::Sqlite, "PRAGMA journal_mode = WAL").risk,
            StatementRisk::WriteDdl
        );
        assert_eq!(
            inspect_statement_for_engine(DbEngine::Sqlite, "EXPLAIN QUERY PLAN SELECT * FROM students").risk,
            StatementRisk::ReadOnly
        );
        assert_eq!(
            inspect_statement_for_engine(DbEngine::Sqlite, "ATTACH DATABASE 'extra.db' AS extra").risk,
            StatementRisk::WriteDdl
        );
        assert_eq!(
            inspect_statement_for_engine(DbEngine::Sqlite, "DETACH DATABASE extra").risk,
            StatementRisk::WriteDdl
        );
        assert_eq!(
            inspect_statement_for_engine(DbEngine::Sqlite, "REINDEX").risk,
            StatementRisk::WriteDdl
        );
        assert_eq!(
            inspect_statement_for_engine(DbEngine::Sqlite, "VACUUM").risk,
            StatementRisk::WriteDdl
        );
    }
}
