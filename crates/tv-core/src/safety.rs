use sqlparser::ast::{
    AccessExpr, AlterColumnOperation, AlterFunctionOperation, AlterIndexOperation, AlterSchemaOperation,
    AlterTable, AlterTableOperation, AlterTypeOperation, AlterViewOperation, Distinct, Expr, FunctionArg,
    FunctionArgExpr, FunctionArguments, GroupByExpr, InTarget, JoinConstraint, JoinOperator, LimitClause,
    ObjectName, ObjectNamePart, OrderByExpr, OrderByKind, PivotValueSource, Query, Select, SelectItem, Set,
    SetExpr, Statement, Subscript, TableFactor, TransactionAccessMode, TransactionMode, UtilityOption, Value,
    WindowType,
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
                if unparsed_pragma_is_setter(stripped) {
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

    if starts_with_keyword(engine, &upper, "DROP")
        || starts_with_keyword(engine, &upper, "TRUNCATE")
        || starts_with_keyword(engine, &upper, "RENAME")
        || is_create_or_replace_command(engine, &upper)
        || (starts_with_keyword(engine, &upper, "ALTER") && contains_destructive_alter_keyword(&upper))
    {
        return StatementInspection {
            sql: trimmed.to_owned(),
            risk: StatementRisk::Destructive,
            summary: "Destructive DDL command".to_owned(),
            parsed: false,
        };
    }

    // Fail-closed on unparseable engine-specific statements so guardrails cannot be bypassed.
    StatementInspection {
        sql: trimmed.to_owned(),
        risk: StatementRisk::WriteDml,
        summary: "Unparsed SQL statement (treated as write for safety)".to_owned(),
        parsed: false,
    }
}

fn is_create_or_replace_command(engine: DbEngine, upper: &str) -> bool {
    if !starts_with_keyword(engine, upper, "CREATE") {
        return false;
    }
    let mut toks = upper
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|t| !t.is_empty());
    matches!(
        (toks.next(), toks.next(), toks.next()),
        (Some("CREATE"), Some("OR"), Some("REPLACE" | "ALTER"))
    )
}

fn contains_destructive_alter_keyword(upper: &str) -> bool {
    upper
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .any(|tok| {
            matches!(
                tok,
                "DROP" | "TRUNCATE" | "DETACH" | "MODIFY" | "CHANGE" | "RENAME" | "TYPE" | "SWAP" | "DISABLE"
            )
        })
}

fn starts_with_keyword(engine: DbEngine, upper: &str, kw: &str) -> bool {
    upper.starts_with(kw)
        && upper[kw.len()..]
            .chars()
            .next()
            .is_none_or(|c| crate::sql::is_sql_whitespace(engine, c) || c == ';')
}

fn is_readonly_sqlite_pragma_ident(raw: &str) -> bool {
    matches!(
        raw.to_ascii_lowercase().as_str(),
        "table_info"
            | "table_xinfo"
            | "index_list"
            | "index_info"
            | "index_xinfo"
            | "foreign_key_list"
            | "foreign_key_check"
            | "integrity_check"
            | "quick_check"
            | "collation_list"
            | "database_list"
            | "function_list"
            | "module_list"
            | "pragma_list"
            | "table_list"
            | "compile_options"
    )
}

fn is_sqlite_readonly_pragma_name(name: &ObjectName) -> bool {
    let Some(last) = name.0.last() else {
        return false;
    };
    let raw = match last {
        ObjectNamePart::Identifier(ident) => ident.value.as_str(),
        ObjectNamePart::Function(_) => return false,
    };
    is_readonly_sqlite_pragma_ident(raw)
}

fn unparsed_pragma_is_setter(sql: &str) -> bool {
    let after_pragma = sql
        .trim_start()
        .get(6..)
        .map_or("", str::trim_start);
    let mut in_single = false;
    let mut in_double = false;
    let mut in_bracket = false;
    let mut first_paren: Option<usize> = None;
    for (idx, ch) in after_pragma.char_indices() {
        match ch {
            '\'' if !in_double && !in_bracket => in_single = !in_single,
            '"' if !in_single && !in_bracket => in_double = !in_double,
            '[' if !in_single && !in_double => in_bracket = true,
            ']' if in_bracket => in_bracket = false,
            '=' if !in_single && !in_double && !in_bracket => return true,
            '(' if !in_single && !in_double && !in_bracket => {
                first_paren = Some(idx);
                break;
            }
            _ => {}
        }
    }
    let Some(paren_idx) = first_paren else {
        return false;
    };
    let head = after_pragma[..paren_idx].trim();
    let target = head.rsplit('.').next().unwrap_or(head).trim();
    let unquoted = target
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .or_else(|| target.strip_prefix('"').and_then(|s| s.strip_suffix('"')))
        .or_else(|| target.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .or_else(|| target.strip_prefix('`').and_then(|s| s.strip_suffix('`')))
        .unwrap_or(target);
    !is_readonly_sqlite_pragma_ident(unquoted)
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
        Statement::Pragma { name, is_eq, value } => {
            if *is_eq || (value.is_some() && !is_sqlite_readonly_pragma_name(name)) {
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
        | Statement::DropSecret { .. }
        | Statement::DropPolicy(_)
        | Statement::DropRule(_)
        | Statement::DropConnector { .. }
        | Statement::DropExtension(_)
        | Statement::DropOperator(_)
        | Statement::DropOperatorFamily(_)
        | Statement::DropOperatorClass(_)
        | Statement::DropTrigger(_) => (StatementRisk::Destructive, "DROP".to_owned()),
        Statement::RenameTable(_) => (StatementRisk::Destructive, "RENAME TABLE".to_owned()),
        Statement::CreateTable(ct) if ct.or_replace => {
            (StatementRisk::Destructive, "CREATE OR REPLACE TABLE".to_owned())
        }
        Statement::CreateView(cv) if cv.or_replace || cv.or_alter => {
            (StatementRisk::Destructive, "CREATE OR REPLACE VIEW".to_owned())
        }
        Statement::CreateFunction(cf) if cf.or_replace || cf.or_alter => {
            (StatementRisk::Destructive, "CREATE OR REPLACE FUNCTION".to_owned())
        }
        Statement::CreateTrigger(ct) if ct.or_replace || ct.or_alter => {
            (StatementRisk::Destructive, "CREATE OR REPLACE TRIGGER".to_owned())
        }
        Statement::CreateRule(cr) if cr.or_replace => {
            (StatementRisk::Destructive, "CREATE OR REPLACE RULE".to_owned())
        }
        Statement::CreateAggregate(ca) if ca.or_replace => {
            (StatementRisk::Destructive, "CREATE OR REPLACE AGGREGATE".to_owned())
        }
        Statement::CreateSchema {
            or_replace: true, ..
        } => (StatementRisk::Destructive, "CREATE OR REPLACE SCHEMA".to_owned()),
        Statement::CreateSecret {
            or_replace: true, ..
        } => (StatementRisk::Destructive, "CREATE OR REPLACE SECRET".to_owned()),
        Statement::CreateMacro {
            or_replace: true, ..
        } => (StatementRisk::Destructive, "CREATE OR REPLACE MACRO".to_owned()),
        Statement::CreateStage {
            or_replace: true, ..
        } => (StatementRisk::Destructive, "CREATE OR REPLACE STAGE".to_owned()),
        Statement::CreateFileFormat {
            or_replace: true, ..
        } => (
            StatementRisk::Destructive,
            "CREATE OR REPLACE FILE FORMAT".to_owned(),
        ),
        Statement::CreateProcedure { or_alter: true, .. } => {
            (StatementRisk::Destructive, "CREATE OR ALTER PROCEDURE".to_owned())
        }
        Statement::AlterIndex {
            operation: AlterIndexOperation::RenameIndex { .. },
            ..
        } => (StatementRisk::Destructive, "ALTER INDEX RENAME".to_owned()),
        Statement::AlterFunction(af)
            if matches!(
                af.operation,
                AlterFunctionOperation::RenameTo { .. } | AlterFunctionOperation::SetSchema { .. }
            ) =>
        {
            (StatementRisk::Destructive, "ALTER FUNCTION (RENAME / MOVE)".to_owned())
        }
        Statement::AlterTable(alter) => {
            if let Some(summary) = alter_table_is_destructive(alter) {
                (StatementRisk::Destructive, summary.to_owned())
            } else {
                (StatementRisk::WriteDdl, "ALTER TABLE".to_owned())
            }
        }
        Statement::AlterSchema(alter) => {
            let destructive = alter.operations.iter().any(|op| {
                matches!(
                    op,
                    AlterSchemaOperation::DropReplica { .. } | AlterSchemaOperation::Rename { .. }
                )
            });
            if destructive {
                (StatementRisk::Destructive, "ALTER SCHEMA (DROP / RENAME)".to_owned())
            } else {
                (StatementRisk::WriteDdl, "ALTER SCHEMA".to_owned())
            }
        }
        Statement::AlterView(alter) => {
            let destructive = matches!(
                alter.operation,
                AlterViewOperation::RenameColumn { .. }
                    | AlterViewOperation::RenameTo { .. }
                    | AlterViewOperation::SetSchema { .. }
            );
            if destructive {
                (StatementRisk::Destructive, "ALTER VIEW (RENAME / MOVE)".to_owned())
            } else {
                (StatementRisk::WriteDdl, "ALTER VIEW".to_owned())
            }
        }
        Statement::AlterType(alter) => {
            let destructive = matches!(
                alter.operation,
                AlterTypeOperation::Rename(_) | AlterTypeOperation::RenameValue(_)
            );
            if destructive {
                (StatementRisk::Destructive, "ALTER TYPE (RENAME)".to_owned())
            } else {
                (StatementRisk::WriteDdl, "ALTER TYPE".to_owned())
            }
        }
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
        | Statement::AlterRole { .. }
        | Statement::AlterPolicy(_)
        | Statement::AlterFunction(_)
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

fn alter_table_is_destructive(alter: &AlterTable) -> Option<&'static str> {
    for op in &alter.operations {
        match op {
            AlterTableOperation::DropColumn { .. }
            | AlterTableOperation::DropConstraint { .. }
            | AlterTableOperation::DropPrimaryKey { .. }
            | AlterTableOperation::DropForeignKey { .. }
            | AlterTableOperation::DropIndex { .. }
            | AlterTableOperation::DropPartitions { .. }
            | AlterTableOperation::DropProjection { .. }
            | AlterTableOperation::DropClusteringKey => {
                return Some("ALTER TABLE DROP");
            }
            AlterTableOperation::DetachPartition { .. }
            | AlterTableOperation::DetachPartitionOf { .. } => {
                return Some("ALTER TABLE DETACH PARTITION");
            }
            AlterTableOperation::AlterColumn {
                op: AlterColumnOperation::SetDataType { .. },
                ..
            } => {
                return Some("ALTER TABLE ALTER COLUMN TYPE");
            }
            AlterTableOperation::ChangeColumn { .. } | AlterTableOperation::ModifyColumn { .. } => {
                return Some("ALTER TABLE MODIFY / CHANGE COLUMN");
            }
            AlterTableOperation::RenameColumn { .. }
            | AlterTableOperation::RenameTable { .. }
            | AlterTableOperation::RenamePartitions { .. }
            | AlterTableOperation::RenameConstraint { .. }
            | AlterTableOperation::SwapWith { .. } => {
                return Some("ALTER TABLE RENAME / SWAP");
            }
            AlterTableOperation::DisableRowLevelSecurity
            | AlterTableOperation::NoForceRowLevelSecurity
            | AlterTableOperation::DisableRule { .. }
            | AlterTableOperation::DisableTrigger { .. } => {
                return Some("ALTER TABLE DISABLE SECURITY / TRIGGER");
            }
            _ => {}
        }
    }
    None
}

fn escalate_risk(worst: &mut (StatementRisk, String), candidate: (StatementRisk, String)) {
    if candidate.0 > worst.0 {
        *worst = candidate;
    }
}

fn classify_query(engine: DbEngine, query: &Query, raw_sql: &str) -> (StatementRisk, String) {
    let mut worst = (StatementRisk::ReadOnly, "SELECT".to_owned());

    if let Some(with) = &query.with {
        for cte in &with.cte_tables {
            let (risk, summary) = classify_query(engine, &cte.query, raw_sql);
            escalate_risk(&mut worst, (risk, format!("WITH CTE ({summary})")));
        }
    }

    escalate_risk(&mut worst, classify_set_expr(engine, &query.body, raw_sql));

    if let Some(order_by) = &query.order_by
        && let OrderByKind::Expressions(exprs) = &order_by.kind
    {
        classify_order_by_exprs(engine, exprs, raw_sql, &mut worst);
    }
    if let Some(limit_clause) = &query.limit_clause {
        match limit_clause {
            LimitClause::LimitOffset {
                limit,
                offset,
                limit_by,
            } => {
                if let Some(e) = limit {
                    escalate_risk(&mut worst, classify_expr_subqueries(engine, e, raw_sql));
                }
                if let Some(o) = offset {
                    escalate_risk(&mut worst, classify_expr_subqueries(engine, &o.value, raw_sql));
                }
                for e in limit_by {
                    escalate_risk(&mut worst, classify_expr_subqueries(engine, e, raw_sql));
                }
            }
            LimitClause::OffsetCommaLimit { offset, limit } => {
                escalate_risk(&mut worst, classify_expr_subqueries(engine, offset, raw_sql));
                escalate_risk(&mut worst, classify_expr_subqueries(engine, limit, raw_sql));
            }
        }
    }
    if let Some(fetch) = &query.fetch
        && let Some(qty) = &fetch.quantity
    {
        escalate_risk(&mut worst, classify_expr_subqueries(engine, qty, raw_sql));
    }

    if !query.locks.is_empty() {
        escalate_risk(
            &mut worst,
            (StatementRisk::WriteDml, "SELECT FOR UPDATE / LOCKING READ".to_owned()),
        );
    }

    worst
}

fn classify_order_by_exprs(
    engine: DbEngine,
    exprs: &[OrderByExpr],
    raw_sql: &str,
    worst: &mut (StatementRisk, String),
) {
    for obe in exprs {
        escalate_risk(worst, classify_expr_subqueries(engine, &obe.expr, raw_sql));
    }
}

fn classify_function_arg(
    engine: DbEngine,
    arg: &FunctionArg,
    raw_sql: &str,
    worst: &mut (StatementRisk, String),
) {
    match arg {
        FunctionArg::Unnamed(FunctionArgExpr::Expr(e))
        | FunctionArg::Named {
            arg: FunctionArgExpr::Expr(e),
            ..
        } => {
            escalate_risk(worst, classify_expr_subqueries(engine, e, raw_sql));
        }
        FunctionArg::ExprNamed { name, arg, .. } => {
            escalate_risk(worst, classify_expr_subqueries(engine, name, raw_sql));
            if let FunctionArgExpr::Expr(e) = arg {
                escalate_risk(worst, classify_expr_subqueries(engine, e, raw_sql));
            }
        }
        _ => {}
    }
}

fn classify_function_args(
    engine: DbEngine,
    args: &FunctionArguments,
    raw_sql: &str,
    worst: &mut (StatementRisk, String),
) {
    if let FunctionArguments::List(list) = args {
        for arg in &list.args {
            classify_function_arg(engine, arg, raw_sql, worst);
        }
    }
}

fn classify_join_constraint(
    engine: DbEngine,
    constraint: &JoinConstraint,
    raw_sql: &str,
    worst: &mut (StatementRisk, String),
) {
    if let JoinConstraint::On(expr) = constraint {
        escalate_risk(worst, classify_expr_subqueries(engine, expr, raw_sql));
    }
}

fn classify_join_operator(engine: DbEngine, op: &JoinOperator, raw_sql: &str) -> (StatementRisk, String) {
    let mut worst = (StatementRisk::ReadOnly, "SELECT".to_owned());
    match op {
        JoinOperator::Join(c)
        | JoinOperator::Inner(c)
        | JoinOperator::Left(c)
        | JoinOperator::LeftOuter(c)
        | JoinOperator::Right(c)
        | JoinOperator::RightOuter(c)
        | JoinOperator::FullOuter(c)
        | JoinOperator::CrossJoin(c)
        | JoinOperator::Semi(c)
        | JoinOperator::LeftSemi(c)
        | JoinOperator::RightSemi(c)
        | JoinOperator::Anti(c)
        | JoinOperator::LeftAnti(c)
        | JoinOperator::RightAnti(c)
        | JoinOperator::StraightJoin(c) => classify_join_constraint(engine, c, raw_sql, &mut worst),
        JoinOperator::AsOf {
            match_condition,
            constraint,
        } => {
            escalate_risk(&mut worst, classify_expr_subqueries(engine, match_condition, raw_sql));
            classify_join_constraint(engine, constraint, raw_sql, &mut worst);
        }
        JoinOperator::CrossApply
        | JoinOperator::OuterApply
        | JoinOperator::ArrayJoin
        | JoinOperator::LeftArrayJoin
        | JoinOperator::InnerArrayJoin => {}
    }
    worst
}

fn classify_table_factor(engine: DbEngine, factor: &TableFactor, raw_sql: &str) -> (StatementRisk, String) {
    let mut worst = (StatementRisk::ReadOnly, "SELECT".to_owned());
    match factor {
        TableFactor::Derived { subquery, .. } => {
            escalate_risk(&mut worst, classify_query(engine, subquery, raw_sql));
        }
        TableFactor::NestedJoin { table_with_joins, .. } => {
            escalate_risk(
                &mut worst,
                classify_table_factor(engine, &table_with_joins.relation, raw_sql),
            );
            for join in &table_with_joins.joins {
                escalate_risk(
                    &mut worst,
                    classify_join_operator(engine, &join.join_operator, raw_sql),
                );
                escalate_risk(&mut worst, classify_table_factor(engine, &join.relation, raw_sql));
            }
        }
        TableFactor::Table {
            args, with_hints, ..
        } => {
            if let Some(tfa) = args {
                for arg in &tfa.args {
                    classify_function_arg(engine, arg, raw_sql, &mut worst);
                }
            }
            for hint in with_hints {
                escalate_risk(&mut worst, classify_expr_subqueries(engine, hint, raw_sql));
            }
        }
        TableFactor::TableFunction { expr, .. } | TableFactor::JsonTable { json_expr: expr, .. }
        | TableFactor::OpenJsonTable { json_expr: expr, .. } => {
            escalate_risk(&mut worst, classify_expr_subqueries(engine, expr, raw_sql));
        }
        TableFactor::Function { args, .. } => {
            for arg in args {
                classify_function_arg(engine, arg, raw_sql, &mut worst);
            }
        }
        TableFactor::UNNEST { array_exprs, .. } => {
            for expr in array_exprs {
                escalate_risk(&mut worst, classify_expr_subqueries(engine, expr, raw_sql));
            }
        }
        TableFactor::Pivot {
            table,
            aggregate_functions,
            value_column,
            value_source,
            default_on_null,
            ..
        } => {
            escalate_risk(&mut worst, classify_table_factor(engine, table, raw_sql));
            for af in aggregate_functions {
                escalate_risk(&mut worst, classify_expr_subqueries(engine, &af.expr, raw_sql));
            }
            for vc in value_column {
                escalate_risk(&mut worst, classify_expr_subqueries(engine, vc, raw_sql));
            }
            match value_source {
                PivotValueSource::List(items) => {
                    for item in items {
                        escalate_risk(&mut worst, classify_expr_subqueries(engine, &item.expr, raw_sql));
                    }
                }
                PivotValueSource::Any(exprs) => classify_order_by_exprs(engine, exprs, raw_sql, &mut worst),
                PivotValueSource::Subquery(q) => {
                    escalate_risk(&mut worst, classify_query(engine, q, raw_sql));
                }
            }
            if let Some(def) = default_on_null {
                escalate_risk(&mut worst, classify_expr_subqueries(engine, def, raw_sql));
            }
        }
        TableFactor::Unpivot { table, value, .. } => {
            escalate_risk(&mut worst, classify_table_factor(engine, table, raw_sql));
            escalate_risk(&mut worst, classify_expr_subqueries(engine, value, raw_sql));
        }
        TableFactor::UnpivotExpr { expression, .. } => {
            escalate_risk(&mut worst, classify_expr_subqueries(engine, expression, raw_sql));
        }
        TableFactor::MatchRecognize {
            table,
            partition_by,
            order_by,
            measures,
            ..
        } => {
            escalate_risk(&mut worst, classify_table_factor(engine, table, raw_sql));
            for e in partition_by {
                escalate_risk(&mut worst, classify_expr_subqueries(engine, e, raw_sql));
            }
            classify_order_by_exprs(engine, order_by, raw_sql, &mut worst);
            for m in measures {
                escalate_risk(&mut worst, classify_expr_subqueries(engine, &m.expr, raw_sql));
            }
        }
        TableFactor::XmlTable {
            row_expression,
            passing,
            ..
        } => {
            escalate_risk(&mut worst, classify_expr_subqueries(engine, row_expression, raw_sql));
            for arg in &passing.arguments {
                escalate_risk(&mut worst, classify_expr_subqueries(engine, &arg.expr, raw_sql));
            }
        }
        TableFactor::SemanticView {
            dimensions,
            metrics,
            facts,
            where_clause,
            ..
        } => {
            for e in dimensions.iter().chain(metrics.iter()).chain(facts.iter()) {
                escalate_risk(&mut worst, classify_expr_subqueries(engine, e, raw_sql));
            }
            if let Some(w) = where_clause {
                escalate_risk(&mut worst, classify_expr_subqueries(engine, w, raw_sql));
            }
        }
    }
    worst
}

fn classify_expr_subqueries(engine: DbEngine, expr: &Expr, raw_sql: &str) -> (StatementRisk, String) {
    let mut worst = (StatementRisk::ReadOnly, "SELECT".to_owned());
    match expr {
        Expr::Subquery(q) | Expr::Exists { subquery: q, .. } => {
            escalate_risk(&mut worst, classify_query(engine, q, raw_sql));
        }
        Expr::In { expr, target, .. } => {
            escalate_risk(&mut worst, classify_expr_subqueries(engine, expr, raw_sql));
            match target {
                InTarget::List(list) => {
                    for item in list {
                        escalate_risk(&mut worst, classify_expr_subqueries(engine, item, raw_sql));
                    }
                }
                InTarget::Subquery(subquery) => {
                    escalate_risk(&mut worst, classify_query(engine, subquery, raw_sql));
                }
                InTarget::Unnest(array_expr) => {
                    escalate_risk(&mut worst, classify_expr_subqueries(engine, array_expr, raw_sql));
                }
                InTarget::Table {
                    args: Some(args), ..
                } => {
                    for arg in args {
                        classify_function_arg(engine, arg, raw_sql, &mut worst);
                    }
                }
                InTarget::Table { args: None, .. } => {}
            }
        }
        Expr::BinaryOp { left, right, .. }
        | Expr::AnyOp { left, right, .. }
        | Expr::AllOp { left, right, .. }
        | Expr::IsDistinctFrom(left, right)
        | Expr::IsNotDistinctFrom(left, right)
        | Expr::AtTimeZone {
            timestamp: left,
            time_zone: right,
        }
        | Expr::Position {
            expr: left,
            r#in: right,
        }
        | Expr::RLike {
            expr: left,
            pattern: right,
            ..
        } => {
            escalate_risk(&mut worst, classify_expr_subqueries(engine, left, raw_sql));
            escalate_risk(&mut worst, classify_expr_subqueries(engine, right, raw_sql));
        }
        Expr::Like {
            expr,
            pattern,
            escape_char,
            ..
        }
        | Expr::ILike {
            expr,
            pattern,
            escape_char,
            ..
        }
        | Expr::SimilarTo {
            expr,
            pattern,
            escape_char,
            ..
        } => {
            escalate_risk(&mut worst, classify_expr_subqueries(engine, expr, raw_sql));
            escalate_risk(&mut worst, classify_expr_subqueries(engine, pattern, raw_sql));
            if let Some(esc) = escape_char {
                escalate_risk(&mut worst, classify_expr_subqueries(engine, esc, raw_sql));
            }
        }
        Expr::UnaryOp { expr, .. }
        | Expr::Nested(expr)
        | Expr::Cast { expr, .. }
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr)
        | Expr::IsTrue(expr)
        | Expr::IsNotTrue(expr)
        | Expr::IsFalse(expr)
        | Expr::IsNotFalse(expr)
        | Expr::IsUnknown(expr)
        | Expr::IsNotUnknown(expr)
        | Expr::IsJson { expr, .. }
        | Expr::IsNormalized { expr, .. }
        | Expr::OuterJoin(expr)
        | Expr::Prior(expr)
        | Expr::Collate { expr, .. }
        | Expr::Ceil { expr, .. }
        | Expr::Floor { expr, .. }
        | Expr::Extract { expr, .. }
        | Expr::Prefixed { value: expr, .. }
        | Expr::Named { expr, .. }
        | Expr::JsonAccess { value: expr, .. } => {
            escalate_risk(&mut worst, classify_expr_subqueries(engine, expr, raw_sql));
        }
        Expr::Convert { expr, styles, .. } => {
            escalate_risk(&mut worst, classify_expr_subqueries(engine, expr, raw_sql));
            for style in styles {
                escalate_risk(&mut worst, classify_expr_subqueries(engine, style, raw_sql));
            }
        }
        Expr::Substring {
            expr,
            substring_from,
            substring_for,
            ..
        } => {
            escalate_risk(&mut worst, classify_expr_subqueries(engine, expr, raw_sql));
            if let Some(from_e) = substring_from {
                escalate_risk(&mut worst, classify_expr_subqueries(engine, from_e, raw_sql));
            }
            if let Some(for_e) = substring_for {
                escalate_risk(&mut worst, classify_expr_subqueries(engine, for_e, raw_sql));
            }
        }
        Expr::Trim {
            expr,
            trim_what,
            trim_characters,
            ..
        } => {
            escalate_risk(&mut worst, classify_expr_subqueries(engine, expr, raw_sql));
            if let Some(what) = trim_what {
                escalate_risk(&mut worst, classify_expr_subqueries(engine, what, raw_sql));
            }
            if let Some(chars) = trim_characters {
                for c in chars {
                    escalate_risk(&mut worst, classify_expr_subqueries(engine, c, raw_sql));
                }
            }
        }
        Expr::Overlay {
            expr,
            overlay_what,
            overlay_from,
            overlay_for,
        } => {
            escalate_risk(&mut worst, classify_expr_subqueries(engine, expr, raw_sql));
            escalate_risk(&mut worst, classify_expr_subqueries(engine, overlay_what, raw_sql));
            escalate_risk(&mut worst, classify_expr_subqueries(engine, overlay_from, raw_sql));
            if let Some(for_e) = overlay_for {
                escalate_risk(&mut worst, classify_expr_subqueries(engine, for_e, raw_sql));
            }
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            escalate_risk(&mut worst, classify_expr_subqueries(engine, expr, raw_sql));
            escalate_risk(&mut worst, classify_expr_subqueries(engine, low, raw_sql));
            escalate_risk(&mut worst, classify_expr_subqueries(engine, high, raw_sql));
        }
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            if let Some(op) = operand {
                escalate_risk(&mut worst, classify_expr_subqueries(engine, op, raw_sql));
            }
            for cw in conditions {
                escalate_risk(&mut worst, classify_expr_subqueries(engine, &cw.condition, raw_sql));
                escalate_risk(&mut worst, classify_expr_subqueries(engine, &cw.result, raw_sql));
            }
            if let Some(els) = else_result {
                escalate_risk(&mut worst, classify_expr_subqueries(engine, els, raw_sql));
            }
        }
        Expr::Function(func) => {
            classify_function_args(engine, &func.parameters, raw_sql, &mut worst);
            classify_function_args(engine, &func.args, raw_sql, &mut worst);
            classify_order_by_exprs(engine, &func.within_group, raw_sql, &mut worst);
            if let Some(filter) = &func.filter {
                escalate_risk(&mut worst, classify_expr_subqueries(engine, filter, raw_sql));
            }
            if let Some(WindowType::WindowSpec(spec)) = &func.over {
                for p in &spec.partition_by {
                    escalate_risk(&mut worst, classify_expr_subqueries(engine, p, raw_sql));
                }
                classify_order_by_exprs(engine, &spec.order_by, raw_sql, &mut worst);
            }
        }
        Expr::Tuple(items) | Expr::Struct { values: items, .. } => {
            for item in items {
                escalate_risk(&mut worst, classify_expr_subqueries(engine, item, raw_sql));
            }
        }
        Expr::Array(arr) => {
            for item in &arr.elem {
                escalate_risk(&mut worst, classify_expr_subqueries(engine, item, raw_sql));
            }
        }
        Expr::Dictionary(fields) => {
            for field in fields {
                escalate_risk(&mut worst, classify_expr_subqueries(engine, &field.value, raw_sql));
            }
        }
        Expr::GroupingSets(sets) | Expr::Cube(sets) | Expr::Rollup(sets) => {
            for group in sets {
                for item in group {
                    escalate_risk(&mut worst, classify_expr_subqueries(engine, item, raw_sql));
                }
            }
        }
        Expr::CompoundFieldAccess { root, access_chain } => {
            escalate_risk(&mut worst, classify_expr_subqueries(engine, root, raw_sql));
            for access in access_chain {
                match access {
                    AccessExpr::Dot(e) => {
                        escalate_risk(&mut worst, classify_expr_subqueries(engine, e, raw_sql));
                    }
                    AccessExpr::Subscript(Subscript::Index { index }) => {
                        escalate_risk(&mut worst, classify_expr_subqueries(engine, index, raw_sql));
                    }
                    AccessExpr::Subscript(Subscript::Slice {
                        lower_bound,
                        upper_bound,
                        stride,
                    }) => {
                        for bound in [lower_bound, upper_bound, stride].into_iter().flatten() {
                            escalate_risk(&mut worst, classify_expr_subqueries(engine, bound, raw_sql));
                        }
                    }
                }
            }
        }
        Expr::Interval(iv) => {
            escalate_risk(&mut worst, classify_expr_subqueries(engine, &iv.value, raw_sql));
        }
        Expr::Lambda(lf) => {
            escalate_risk(&mut worst, classify_expr_subqueries(engine, &lf.body, raw_sql));
        }
        _ => {}
    }
    worst
}

fn classify_select_subqueries(engine: DbEngine, sel: &Select, raw_sql: &str) -> (StatementRisk, String) {
    let mut worst = if sel.into.is_some() {
        (StatementRisk::WriteDml, "SELECT INTO".to_owned())
    } else {
        (StatementRisk::ReadOnly, "SELECT".to_owned())
    };
    if let Some(Distinct::On(exprs)) = &sel.distinct {
        for e in exprs {
            escalate_risk(&mut worst, classify_expr_subqueries(engine, e, raw_sql));
        }
    }
    for twj in &sel.from {
        escalate_risk(&mut worst, classify_table_factor(engine, &twj.relation, raw_sql));
        for join in &twj.joins {
            escalate_risk(
                &mut worst,
                classify_join_operator(engine, &join.join_operator, raw_sql),
            );
            escalate_risk(&mut worst, classify_table_factor(engine, &join.relation, raw_sql));
        }
    }
    for lv in &sel.lateral_views {
        escalate_risk(
            &mut worst,
            classify_expr_subqueries(engine, &lv.lateral_view, raw_sql),
        );
    }
    for item in &sel.projection {
        match item {
            SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => {
                escalate_risk(&mut worst, classify_expr_subqueries(engine, e, raw_sql));
            }
            _ => {}
        }
    }
    if let Some(prewhere) = &sel.prewhere {
        escalate_risk(&mut worst, classify_expr_subqueries(engine, prewhere, raw_sql));
    }
    if let Some(selection) = &sel.selection {
        escalate_risk(&mut worst, classify_expr_subqueries(engine, selection, raw_sql));
    }
    if let GroupByExpr::Expressions(exprs, _) = &sel.group_by {
        for e in exprs {
            escalate_risk(&mut worst, classify_expr_subqueries(engine, e, raw_sql));
        }
    }
    for e in sel
        .cluster_by
        .iter()
        .chain(sel.distribute_by.iter())
        .chain(sel.having.iter())
        .chain(sel.qualify.iter())
    {
        escalate_risk(&mut worst, classify_expr_subqueries(engine, e, raw_sql));
    }
    classify_order_by_exprs(engine, &sel.sort_by, raw_sql, &mut worst);
    worst
}

fn classify_set_expr(engine: DbEngine, expr: &SetExpr, raw_sql: &str) -> (StatementRisk, String) {
    match expr {
        SetExpr::Select(sel) => classify_select_subqueries(engine, sel, raw_sql),
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
        SetExpr::Values(values) => {
            let mut worst = (StatementRisk::ReadOnly, "VALUES".to_owned());
            for row in &values.rows {
                for expr in row.iter() {
                    escalate_risk(&mut worst, classify_expr_subqueries(engine, expr, raw_sql));
                }
            }
            worst
        }
        SetExpr::Table(_) => (StatementRisk::ReadOnly, "SELECT".to_owned()),
        SetExpr::Insert(s) | SetExpr::Update(s) | SetExpr::Delete(s) | SetExpr::Merge(s) => {
            classify_ast_statement(engine, s, raw_sql)
        }
    }
}

/// Validates a user-supplied table `WHERE` filter so it cannot escape `WHERE (...)`, stack
/// statements, or embed writes, DDL, locking reads, or file output clauses.
pub fn validate_table_filter_where(
    engine: DbEngine,
    filter: &str,
    session_mode: crate::sql::SqlSessionMode,
) -> Result<(), String> {
    let trimmed = trim_sql_whitespace(engine, filter.strip_prefix('\u{feff}').unwrap_or(filter));
    if trimmed.is_empty() {
        return Ok(());
    }

    scan_table_filter_tokens(engine, trimmed, &session_mode, false)?;

    let parse_inner: String = match engine {
        DbEngine::Mysql => {
            let mode = session_mode.mysql.unwrap_or(crate::sql::MysqlQuoteMode::DEFAULT);
            crate::sql::mysql_normalize_for_parser(trimmed, mode)
        }
        DbEngine::Postgres if !session_mode.pg_standard_conforming_strings() => {
            crate::sql::pg_normalize_legacy_escape_strings(trimmed).into_owned()
        }
        _ => trimmed.to_owned(),
    };
    let wrapped = format!("SELECT 1 FROM _tv_filter_target WHERE ({parse_inner}\n)");
    let parsed = match engine {
        DbEngine::Postgres => Parser::parse_sql(&PostgreSqlDialect {}, &wrapped),
        DbEngine::Mysql => Parser::parse_sql(&MySqlDialect {}, &wrapped),
        DbEngine::Sqlite => Parser::parse_sql(&SQLiteDialect {}, &wrapped),
    };

    if let Ok(stmts) = parsed {
        let Some(stmt) = stmts.first().filter(|_| stmts.len() == 1) else {
            return Err("Table filter must be a single WHERE expression".to_owned());
        };
        let Statement::Query(query) = stmt else {
            return Err("Table filter must be a read-only WHERE predicate".to_owned());
        };
        if query.with.is_some()
            || !query.locks.is_empty()
            || query.order_by.is_some()
            || query.limit_clause.is_some()
            || query.fetch.is_some()
            || !matches!(query.body.as_ref(), SetExpr::Select(sel) if sel.into.is_none())
        {
            return Err("Table filter must be a read-only WHERE predicate".to_owned());
        }
        if classify_ast_statement(engine, stmt, &wrapped).0 != StatementRisk::ReadOnly {
            return Err("Table filter cannot contain mutating or locking SQL".to_owned());
        }
    } else {
        scan_table_filter_tokens(engine, trimmed, &session_mode, true)?;
        if engine == DbEngine::Mysql
            && mysql_valid_select_into_outfile_or_dumpfile(&wrapped, session_mode.mysql.unwrap_or_default())
        {
            return Err("Table filter cannot write to OUTFILE or DUMPFILE".to_owned());
        }
    }

    Ok(())
}

fn parse_pg_dollar_tag(s: &str, start: usize) -> Option<&str> {
    let bytes = s.as_bytes();
    if bytes.get(start) != Some(&b'$') {
        return None;
    }
    let mut i = start + 1;
    if i < bytes.len() && bytes[i] == b'$' {
        return Some(&s[start..=i]);
    }
    if i >= bytes.len() || !(bytes[i].is_ascii_alphabetic() || bytes[i] == b'_') {
        return None;
    }
    i += 1;
    while i < bytes.len() {
        if bytes[i] == b'$' {
            return Some(&s[start..=i]);
        }
        if !(bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
            return None;
        }
        i += 1;
    }
    None
}

fn scan_table_filter_tokens(
    engine: DbEngine,
    sql: &str,
    session_mode: &crate::sql::SqlSessionMode,
    enforce_unparsed_keywords: bool,
) -> Result<(), String> {
    let bytes = sql.as_bytes();
    let len = bytes.len();
    let mut i = 0usize;
    let mut paren_depth = 0usize;
    let mysql_bs = engine == DbEngine::Mysql && !session_mode.mysql.is_some_and(|m| m.no_backslash_escapes);
    let pg_legacy_bs = engine == DbEngine::Postgres && !session_mode.pg_standard_conforming_strings();
    let ansi_quotes = engine != DbEngine::Mysql || session_mode.mysql.is_some_and(|m| m.ansi_quotes);
    let allow_brackets =
        engine == DbEngine::Sqlite || (engine == DbEngine::Mysql && session_mode.mysql.is_some_and(|m| m.mssql_quotes));
    let mut prev_any_kw: Option<String> = None;
    let mut mysql_lock_stage = 0u8;

    while i < len {
        let b = bytes[i];
        if crate::sql::is_dash_comment_start(engine, sql, i) {
            i += 2;
            while i < len && bytes[i] != b'\n' && !(engine == DbEngine::Postgres && bytes[i] == b'\r') {
                i += 1;
            }
            if i >= len {
                return Err("Table filter cannot end in an unterminated line comment".to_owned());
            }
            continue;
        }
        if engine == DbEngine::Mysql && b == b'#' {
            i += 1;
            while i < len && bytes[i] != b'\n' {
                i += 1;
            }
            if i >= len {
                return Err("Table filter cannot end in an unterminated '#' comment".to_owned());
            }
            continue;
        }
        if i + 1 < len && b == b'/' && bytes[i + 1] == b'*' {
            if engine == DbEngine::Mysql && (sql[i..].starts_with("/*!") || sql[i..].starts_with("/*M!")) {
                return Err("Executable comments are not allowed in table filters".to_owned());
            }
            i += 2;
            let mut depth = 1usize;
            while i < len && depth > 0 {
                if engine == DbEngine::Postgres && i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
                    depth += 1;
                    i += 2;
                } else if i + 1 < len && bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    depth -= 1;
                    i += 2;
                } else {
                    i += sql[i..].chars().next().map_or(1, char::len_utf8);
                }
            }
            if depth > 0 {
                return Err("Table filter contains an unclosed block comment".to_owned());
            }
            continue;
        }
        if b == b'\'' {
            let escaped_prefix = engine == DbEngine::Postgres
                && i > 0
                && matches!(bytes[i - 1], b'e' | b'E')
                && (i == 1 || !(bytes[i - 2].is_ascii_alphanumeric() || bytes[i - 2] == b'_'));
            let allow_bs = mysql_bs || pg_legacy_bs || escaped_prefix;
            i += 1;
            let mut closed = false;
            while i < len {
                if allow_bs && bytes[i] == b'\\' {
                    i += 1 + sql[i + 1..].chars().next().map_or(0, char::len_utf8);
                } else if bytes[i] == b'\'' {
                    if i + 1 < len && bytes[i + 1] == b'\'' {
                        i += 2;
                    } else {
                        i += 1;
                        closed = true;
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            if !closed {
                return Err("Table filter contains an unclosed string literal".to_owned());
            }
            prev_any_kw = None;
            mysql_lock_stage = 0;
            continue;
        }
        if b == b'"' {
            let allow_bs = !ansi_quotes && mysql_bs;
            i += 1;
            let mut closed = false;
            while i < len {
                if allow_bs && bytes[i] == b'\\' {
                    i += 1 + sql[i + 1..].chars().next().map_or(0, char::len_utf8);
                } else if bytes[i] == b'"' {
                    if i + 1 < len && bytes[i + 1] == b'"' {
                        i += 2;
                    } else {
                        i += 1;
                        closed = true;
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            if !closed {
                return Err("Table filter contains an unclosed quoted identifier or literal".to_owned());
            }
            prev_any_kw = None;
            mysql_lock_stage = 0;
            continue;
        }
        if (engine == DbEngine::Mysql || engine == DbEngine::Sqlite) && b == b'`' {
            i += 1;
            let mut closed = false;
            while i < len {
                if bytes[i] == b'`' {
                    if i + 1 < len && bytes[i + 1] == b'`' {
                        i += 2;
                    } else {
                        i += 1;
                        closed = true;
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            if !closed {
                return Err("Table filter contains an unclosed backtick identifier".to_owned());
            }
            prev_any_kw = None;
            mysql_lock_stage = 0;
            continue;
        }
        if allow_brackets && b == b'[' {
            i += 1;
            let mut closed = false;
            while i < len {
                if bytes[i] == b']' {
                    if engine == DbEngine::Mysql && i + 1 < len && bytes[i + 1] == b']' {
                        i += 2;
                    } else {
                        i += 1;
                        closed = true;
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            if !closed {
                return Err("Table filter contains an unclosed bracket identifier".to_owned());
            }
            prev_any_kw = None;
            mysql_lock_stage = 0;
            continue;
        }
        if engine == DbEngine::Postgres
            && b == b'$'
            && let Some(tag) = parse_pg_dollar_tag(sql, i)
        {
            let body_start = i + tag.len();
            let Some(rel_end) = sql[body_start..].find(tag) else {
                return Err("Table filter contains an unclosed dollar-quoted string".to_owned());
            };
            i = body_start + rel_end + tag.len();
            prev_any_kw = None;
            mysql_lock_stage = 0;
            continue;
        }
        if b == b';' {
            return Err("Semicolons are not allowed in table filters".to_owned());
        }
        if b == b'(' {
            paren_depth += 1;
            i += 1;
            prev_any_kw = None;
            mysql_lock_stage = 0;
            continue;
        }
        if b == b')' {
            if paren_depth == 0 {
                return Err("Table filter cannot break out of WHERE (...) with an unmatched ')'".to_owned());
            }
            paren_depth -= 1;
            i += 1;
            prev_any_kw = None;
            mysql_lock_stage = 0;
            continue;
        }
        if b.is_ascii_alphabetic() || b == b'_' {
            let preceded_by_dot = i > 0 && bytes[i - 1] == b'.';
            let start = i;
            i += 1;
            while i < len && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'$') {
                i += 1;
            }
            let followed_by_dot = i < len && bytes[i] == b'.';
            if preceded_by_dot || followed_by_dot {
                prev_any_kw = None;
                mysql_lock_stage = 0;
                continue;
            }
            let kw = sql[start..i].to_ascii_uppercase();
            if enforce_unparsed_keywords
                && matches!(
                    kw.as_str(),
                    "INSERT" | "UPDATE" | "DELETE" | "DROP" | "ALTER" | "TRUNCATE" | "CREATE" | "ATTACH"
                        | "DETACH" | "PRAGMA" | "MERGE" | "REPLACE" | "GRANT" | "REVOKE" | "COPY" | "CALL"
                        | "EXECUTE"
                )
                && !(kw == "UPDATE" && prev_any_kw.as_deref() == Some("FOR"))
            {
                let next_is_paren = sql[i..]
                    .trim_start_matches(|c: char| crate::sql::is_sql_whitespace(engine, c))
                    .starts_with('(');
                if !(kw == "REPLACE" && next_is_paren) {
                    return Err(format!("Keyword '{kw}' is not allowed in a table filter"));
                }
            }
            if prev_any_kw.as_deref() == Some("FOR")
                && matches!(kw.as_str(), "UPDATE" | "SHARE" | "NO" | "KEY")
            {
                return Err("Locking clauses (FOR UPDATE / SHARE) are not allowed in table filters".to_owned());
            }
            if engine == DbEngine::Mysql {
                if mysql_lock_stage == 3 && kw == "MODE" {
                    return Err("Locking clauses (LOCK IN SHARE MODE) are not allowed in table filters".to_owned());
                }
                mysql_lock_stage = match (mysql_lock_stage, kw.as_str()) {
                    (_, "LOCK") => 1,
                    (1, "IN") => 2,
                    (2, "SHARE") => 3,
                    _ => 0,
                };
            }
            if engine == DbEngine::Mysql
                && prev_any_kw.as_deref() == Some("INTO")
                && matches!(kw.as_str(), "OUTFILE" | "DUMPFILE")
            {
                return Err("Table filter cannot write to OUTFILE or DUMPFILE".to_owned());
            }
            if paren_depth == 0
                && matches!(kw.as_str(), "UNION" | "INTERSECT" | "EXCEPT" | "INTO" | "RETURNING")
            {
                return Err(format!("Clause '{kw}' is not allowed in a table filter"));
            }
            prev_any_kw = Some(kw);
            continue;
        }
        let ch = sql[i..].chars().next().unwrap_or('\0');
        i += ch.len_utf8();
        if !crate::sql::is_sql_whitespace(engine, ch) {
            prev_any_kw = None;
            mysql_lock_stage = 0;
        }
    }
    Ok(())
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
