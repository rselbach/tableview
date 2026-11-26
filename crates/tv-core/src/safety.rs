use sqlparser::ast::{
    Query, Set, SetExpr, Statement, TransactionAccessMode, TransactionMode, UtilityOption, Value,
};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;

use crate::connection::{EnvTag, SafetyPolicy};

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
    let trimmed = sql.trim();
    if trimmed.is_empty() {
        return StatementInspection {
            sql: String::new(),
            risk: StatementRisk::ReadOnly,
            summary: "Empty statement".to_owned(),
            parsed: true,
        };
    }

    let dialect = PostgreSqlDialect {};
    match Parser::parse_sql(&dialect, trimmed) {
        Ok(stmts) if !stmts.is_empty() => {
            let mut max_risk = StatementRisk::ReadOnly;
            let mut best_summary = "SELECT".to_owned();
            for stmt in &stmts {
                let (risk, summary) = classify_ast_statement(stmt);
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
        Err(_) => {
            let upper = strip_leading_sql_comments(trimmed).to_ascii_uppercase();
            if upper.starts_with("SHOW ") || upper == "SHOW" || upper.starts_with("VALUES ") {
                StatementInspection {
                    sql: trimmed.to_owned(),
                    risk: StatementRisk::ReadOnly,
                    summary: "SHOW / VALUES".to_owned(),
                    parsed: false,
                }
            } else {
                // Fail-closed on unparseable PostgreSQL-specific statements so guardrails cannot be bypassed.
                StatementInspection {
                    sql: trimmed.to_owned(),
                    risk: StatementRisk::WriteDml,
                    summary: "Unparsed SQL statement (treated as write for safety)".to_owned(),
                    parsed: false,
                }
            }
        }
    }
}

/// Evaluates all SQL statements against the connection's [`SafetyPolicy`] and [`EnvTag`].
pub fn evaluate_sql_safety(policy: SafetyPolicy, env: EnvTag, statements: &[String]) -> SafetyDecision {
    let mut mutating = Vec::new();
    for raw in statements {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let insp = inspect_statement(trimmed);
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

fn classify_ast_statement(stmt: &Statement) -> (StatementRisk, String) {
    match stmt {
        Statement::Query(query) => classify_query(query),
        Statement::Explain {
            analyze,
            statement,
            options,
            ..
        } => {
            let runs_query = *analyze
                || options
                    .as_ref()
                    .is_some_and(|opts| opts.iter().any(is_truthy_analyze_option));
            if runs_query {
                let (inner_risk, inner_summary) = classify_ast_statement(statement);
                if inner_risk == StatementRisk::ReadOnly {
                    (StatementRisk::ReadOnly, format!("EXPLAIN ANALYZE ({inner_summary})"))
                } else {
                    (inner_risk, format!("EXPLAIN ANALYZE ({inner_summary})"))
                }
            } else {
                (StatementRisk::ReadOnly, "EXPLAIN".to_owned())
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
        Statement::Insert(_) => (StatementRisk::WriteDml, "INSERT".to_owned()),
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
        | Statement::Discard { .. } => (StatementRisk::WriteDdl, "DDL / Utility".to_owned()),
        _ => (StatementRisk::WriteDdl, "SQL Command".to_owned()),
    }
}

fn classify_query(query: &Query) -> (StatementRisk, String) {
    let mut worst_risk = StatementRisk::ReadOnly;
    let mut best_summary = "SELECT".to_owned();

    if let Some(with) = &query.with {
        for cte in &with.cte_tables {
            let (risk, summary) = classify_query(&cte.query);
            if risk > worst_risk {
                worst_risk = risk;
                best_summary = format!("WITH CTE ({summary})");
            }
        }
    }

    let (body_risk, body_summary) = classify_set_expr(&query.body);
    if body_risk > worst_risk {
        worst_risk = body_risk;
        best_summary = body_summary;
    }

    (worst_risk, best_summary)
}

fn classify_set_expr(expr: &SetExpr) -> (StatementRisk, String) {
    match expr {
        SetExpr::Select(sel) => {
            if sel.into.is_some() {
                (StatementRisk::WriteDml, "SELECT INTO".to_owned())
            } else {
                (StatementRisk::ReadOnly, "SELECT".to_owned())
            }
        }
        SetExpr::Query(q) => classify_query(q),
        SetExpr::SetOperation { left, right, .. } => {
            let (lr, ls) = classify_set_expr(left);
            let (rr, rs) = classify_set_expr(right);
            if lr >= rr {
                (lr, ls)
            } else {
                (rr, rs)
            }
        }
        SetExpr::Values(_) | SetExpr::Table(_) => (StatementRisk::ReadOnly, "SELECT".to_owned()),
        SetExpr::Insert(s) | SetExpr::Update(s) | SetExpr::Delete(s) | SetExpr::Merge(s) => {
            classify_ast_statement(s)
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

fn strip_leading_sql_comments(mut s: &str) -> &str {
    loop {
        s = s.trim_start();
        if let Some(rest) = s.strip_prefix("--") {
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
