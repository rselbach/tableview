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

/// Inspects a single SQL statement string using the dialect matching `engine`.
pub fn inspect_statement_for_engine(engine: DbEngine, sql: &str) -> StatementInspection {
    let trimmed = sql.trim();
    if trimmed.is_empty() {
        return StatementInspection {
            sql: String::new(),
            risk: StatementRisk::ReadOnly,
            summary: "Empty statement".to_owned(),
            parsed: true,
        };
    }

    let parsed = match engine {
        DbEngine::Postgres => Parser::parse_sql(&PostgreSqlDialect {}, trimmed),
        DbEngine::Mysql => Parser::parse_sql(&MySqlDialect {}, trimmed),
        DbEngine::Sqlite => Parser::parse_sql(&SQLiteDialect {}, trimmed),
    };

    match parsed {
        Ok(stmts) if !stmts.is_empty() => {
            let mut max_risk = StatementRisk::ReadOnly;
            let mut best_summary = "SELECT".to_owned();
            for stmt in &stmts {
                let (risk, summary) = classify_ast_statement(engine, stmt, trimmed);
                if risk > max_risk || max_risk == StatementRisk::ReadOnly {
                    max_risk = risk;
                    best_summary = summary;
                }
            }
            StatementInspection {
                sql: trimmed.to_owned(),
                risk: max_risk,
                summary: best_summary,
                parsed: true,
            }
        }
        Ok(_) => StatementInspection {
            sql: trimmed.to_owned(),
            risk: StatementRisk::ReadOnly,
            summary: "Empty statement".to_owned(),
            parsed: true,
        },
        Err(_) => classify_unparsed_statement(engine, trimmed),
    }
}

fn classify_unparsed_statement(engine: DbEngine, trimmed: &str) -> StatementInspection {
    let stripped = strip_leading_sql_comments(trimmed, engine);
    let upper = stripped.to_ascii_uppercase();

    if starts_with_keyword(&upper, "SHOW") || starts_with_keyword(&upper, "VALUES") {
        return StatementInspection {
            sql: trimmed.to_owned(),
            risk: StatementRisk::ReadOnly,
            summary: "SHOW / VALUES".to_owned(),
            parsed: false,
        };
    }

    match engine {
        DbEngine::Mysql => {
            if starts_with_keyword(&upper, "DESCRIBE") || starts_with_keyword(&upper, "DESC") {
                return StatementInspection {
                    sql: trimmed.to_owned(),
                    risk: StatementRisk::ReadOnly,
                    summary: "DESCRIBE".to_owned(),
                    parsed: false,
                };
            }
            if starts_with_keyword(&upper, "USE") {
                return StatementInspection {
                    sql: trimmed.to_owned(),
                    risk: StatementRisk::ReadOnly,
                    summary: "USE".to_owned(),
                    parsed: false,
                };
            }
            if starts_with_keyword(&upper, "REPLACE") {
                return StatementInspection {
                    sql: trimmed.to_owned(),
                    risk: StatementRisk::WriteDml,
                    summary: "REPLACE".to_owned(),
                    parsed: false,
                };
            }
        }
        DbEngine::Sqlite => {
            if starts_with_keyword(&upper, "PRAGMA") {
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
                    .is_none_or(char::is_whitespace)
            {
                return StatementInspection {
                    sql: trimmed.to_owned(),
                    risk: StatementRisk::ReadOnly,
                    summary: "EXPLAIN QUERY PLAN".to_owned(),
                    parsed: false,
                };
            }
            if starts_with_keyword(&upper, "ATTACH")
                || starts_with_keyword(&upper, "DETACH")
                || starts_with_keyword(&upper, "REINDEX")
                || starts_with_keyword(&upper, "VACUUM")
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

fn starts_with_keyword(upper: &str, kw: &str) -> bool {
    upper.starts_with(kw)
        && upper[kw.len()..]
            .chars()
            .next()
            .is_none_or(|c| c.is_whitespace() || c == ';')
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
    let mut mutating = Vec::new();
    for raw in statements {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let insp = inspect_statement_for_engine(engine, trimmed);
        if insp.risk > StatementRisk::ReadOnly {
            mutating.push(insp);
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
        s = s.trim_start();
        if let Some(rest) = s.strip_prefix("--") {
            match rest.find('\n') {
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
            match rest.find("*/") {
                Some(end) => s = &rest[end + 2..],
                None => return "",
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
