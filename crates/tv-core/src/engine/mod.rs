pub mod postgres;

use crate::explain::ExplainPlan;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use thiserror::Error;
use uuid::Uuid;

/// Metadata reported immediately after connecting to a database server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerInfo {
    pub version: String,
    pub current_database: String,
    pub current_user: String,
}

/// A database catalog available on the connected server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DatabaseInfo {
    pub name: String,
    pub owner: String,
    pub size_bytes: Option<i64>,
}

/// A schema namespace inside the active database.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchemaInfo {
    pub name: String,
    pub is_system: bool,
}

/// Kind of relational object shown in the sidebar catalog.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DbObjectKind {
    Table,
    View,
    MaterializedView,
    ForeignTable,
}

impl DbObjectKind {
    pub const fn badge(self) -> &'static str {
        match self {
            Self::Table => "T",
            Self::View => "V",
            Self::MaterializedView => "MV",
            Self::ForeignTable => "F",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Table => "Table",
            Self::View => "View",
            Self::MaterializedView => "Materialized View",
            Self::ForeignTable => "Foreign Table",
        }
    }
}

/// A table, view, materialized view, or foreign table within a schema.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DbObject {
    pub schema: String,
    pub name: String,
    pub kind: DbObjectKind,
    pub estimated_rows: Option<i64>,
    pub total_bytes: Option<i64>,
}

/// Compact column metadata for sidebar expansion and object summaries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnSummary {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub is_primary_key: bool,
}

/// Foreign key reference attached to a table column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForeignKeyRef {
    pub constraint_name: String,
    pub target_schema: String,
    pub target_table: String,
    pub target_column: String,
}

/// Full structural metadata for a column returned by `describe_table` and grid pagination.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnMeta {
    pub name: String,
    pub data_type: String,
    pub udt_name: String,
    pub nullable: bool,
    pub default_expr: Option<String>,
    pub is_primary_key: bool,
    pub is_unique: bool,
    pub comment: Option<String>,
    pub foreign_key: Option<ForeignKeyRef>,
}

/// Metadata and definition of an index on a table or materialized view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexInfo {
    pub name: String,
    pub columns: Vec<String>,
    pub is_unique: bool,
    pub is_primary: bool,
    pub method: String,
    pub size_bytes: Option<i64>,
    pub definition: String,
}

/// Relation size estimates, comment, and synthesized DDL for a database object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableStats {
    pub total_bytes: Option<i64>,
    pub data_bytes: Option<i64>,
    pub index_bytes: Option<i64>,
    pub estimated_rows: Option<i64>,
    pub comment: Option<String>,
    pub ddl: String,
}

/// Complete structural inspection payload for the Structure and Indexes views and inspector details.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableStructure {
    pub columns: Vec<ColumnMeta>,
    pub indexes: Vec<IndexInfo>,
    pub stats: TableStats,
}

/// Server-side ordering requested on a data grid page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SortSpec {
    pub column: String,
    pub ascending: bool,
}

/// Parameters describing one paginated data fetch from a table or view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageRequest {
    pub schema: String,
    pub table: String,
    pub filter_where: Option<String>,
    pub sort: Option<SortSpec>,
    pub limit: i64,
    pub offset: i64,
}

/// One page of rows decoded into typed [`CellValue`]s alongside column definitions and timing.
#[derive(Clone, Debug, PartialEq)]
pub struct TablePage {
    pub columns: Vec<ColumnMeta>,
    pub rows: Vec<Vec<CellValue>>,
    pub offset: i64,
    pub limit: i64,
    pub total_rows_estimate: Option<i64>,
    pub total_is_exact: bool,
    pub has_more: bool,
    pub elapsed_ms: u64,
}

/// Typed representation of a PostgreSQL column cell preserving `NULL` vs empty strings and wire semantics.
#[derive(Clone, Debug, PartialEq)]
pub enum CellValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Numeric(String),
    Uuid(Uuid),
    Timestamp {
        raw: String,
        utc: Option<DateTime<Utc>>,
    },
    Date(String),
    Time(String),
    Json(String),
    Bytes(Vec<u8>),
    Array(String),
    Text(String),
}

impl CellValue {
    /// Parses a PostgreSQL text-projected cell (`"col"::text`, or `encode("col", 'hex')` for `bytea`)
    /// using the column's underlying PostgreSQL type name (`pg_type.typname`).
    pub fn parse_from_pg_text(udt_name: &str, value: Option<String>) -> Self {
        let Some(s) = value else {
            return Self::Null;
        };
        match udt_name {
            "bool" => match s.as_str() {
                "t" | "true" | "TRUE" => Self::Bool(true),
                "f" | "false" | "FALSE" => Self::Bool(false),
                _ => Self::Text(s),
            },
            "int2" | "int4" | "int8" => match s.parse::<i64>() {
                Ok(v) => Self::Int(v),
                Err(_) => Self::Numeric(s),
            },
            "float4" | "float8" => match s.parse::<f64>() {
                Ok(v) if v.is_finite() => Self::Float(v),
                _ => Self::Text(s),
            },
            "numeric" | "money" => Self::Numeric(s),
            "uuid" => match Uuid::parse_str(&s) {
                Ok(u) => Self::Uuid(u),
                Err(_) => Self::Text(s),
            },
            "timestamptz" | "timestamp" => {
                let utc = parse_pg_timestamp(&s);
                Self::Timestamp { raw: s, utc }
            }
            "date" => Self::Date(s),
            "time" | "timetz" => Self::Time(s),
            "json" | "jsonb" => Self::Json(s),
            "bytea" => match decode_hex(&s) {
                Some(bytes) => Self::Bytes(bytes),
                None => Self::Text(s),
            },
            _ if udt_name.starts_with('_') => Self::Array(s),
            _ => Self::Text(s),
        }
    }

    pub const fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    pub const fn is_right_aligned(&self) -> bool {
        matches!(self, Self::Int(_) | Self::Float(_) | Self::Numeric(_))
    }

    pub const fn is_mono(&self) -> bool {
        matches!(
            self,
            Self::Bool(_)
                | Self::Int(_)
                | Self::Float(_)
                | Self::Numeric(_)
                | Self::Uuid(_)
                | Self::Timestamp { .. }
                | Self::Date(_)
                | Self::Time(_)
                | Self::Json(_)
                | Self::Bytes(_)
                | Self::Array(_)
        )
    }

    /// Formats a single-line compact preview suitable for table cells.
    pub fn display_compact(&self) -> String {
        match self {
            Self::Null => "NULL".to_owned(),
            Self::Bool(b) => if *b { "true" } else { "false" }.to_owned(),
            Self::Int(v) => v.to_string(),
            Self::Float(v) => v.to_string(),
            Self::Numeric(s) | Self::Date(s) | Self::Time(s) | Self::Array(s) => s.clone(),
            Self::Uuid(u) => u.to_string(),
            Self::Timestamp { raw, .. } => raw.clone(),
            Self::Json(s) | Self::Text(s) => collapse_preview(s, 240),
            Self::Bytes(bytes) => {
                if bytes.is_empty() {
                    "\\x (0 B)".to_owned()
                } else {
                    let preview_len = bytes.len().min(16);
                    let hex = encode_hex(&bytes[..preview_len]);
                    if bytes.len() > preview_len {
                        format!("\\x{hex}… ({} B)", bytes.len())
                    } else {
                        format!("\\x{hex} ({} B)", bytes.len())
                    }
                }
            }
        }
    }

    /// Returns full untruncated text suitable for clipboard copy and inspector display.
    pub fn copy_text(&self) -> String {
        match self {
            Self::Null => "NULL".to_owned(),
            Self::Bool(b) => if *b { "true" } else { "false" }.to_owned(),
            Self::Int(v) => v.to_string(),
            Self::Float(v) => v.to_string(),
            Self::Numeric(s)
            | Self::Date(s)
            | Self::Time(s)
            | Self::Array(s)
            | Self::Json(s)
            | Self::Text(s) => s.clone(),
            Self::Uuid(u) => u.to_string(),
            Self::Timestamp { raw, .. } => raw.clone(),
            Self::Bytes(bytes) => format!("\\x{}", encode_hex(bytes)),
        }
    }

    /// Returns editable text suitable for inline grid/inspector editing (`""` for `Null`).
    pub fn to_edit_string(&self) -> String {
        match self {
            Self::Null => String::new(),
            other => other.copy_text(),
        }
    }

    /// Returns pretty-formatted JSON when the cell holds JSON/JSONB.
    pub fn pretty_json(&self) -> Option<String> {
        let Self::Json(raw) = self else {
            return None;
        };
        match serde_json::from_str::<serde_json::Value>(raw) {
            Ok(val) => serde_json::to_string_pretty(&val).ok().or_else(|| Some(raw.clone())),
            Err(_) => Some(raw.clone()),
        }
    }
}

fn collapse_preview(raw: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(raw.len().min(max_chars));
    let mut truncated = false;
    for (count, ch) in raw.chars().enumerate() {
        if count >= max_chars {
            truncated = true;
            break;
        }
        match ch {
            '\n' | '\r' | '\t' => out.push(' '),
            other => out.push(other),
        }
    }
    if truncated {
        out.push('…');
    }
    out
}

pub fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn decode_hex(raw: &str) -> Option<Vec<u8>> {
    let hex = raw.strip_prefix("\\x").unwrap_or(raw);
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    let (chunks, _) = hex.as_bytes().as_chunks::<2>();
    for &pair in chunks {
        let hi = hex_nibble(pair[0])?;
        let lo = hex_nibble(pair[1])?;
        out.push((hi << 4) | lo);
    }
    Some(out)
}

const fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn parse_pg_timestamp(raw: &str) -> Option<DateTime<Utc>> {
    let trimmed = raw.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(trimmed) {
        return Some(dt.with_timezone(&Utc));
    }
    for fmt in [
        "%Y-%m-%d %H:%M:%S%.f%#z",
        "%Y-%m-%d %H:%M:%S%#z",
        "%Y-%m-%dT%H:%M:%S%.f%#z",
        "%Y-%m-%dT%H:%M:%S%#z",
    ] {
        if let Ok(dt) = DateTime::parse_from_str(trimmed, fmt) {
            return Some(dt.with_timezone(&Utc));
        }
    }
    // Naive timestamp intentionally yields `None` rather than fabricating a timezone.
    None
}

/// Typed catalog and introspection errors emitted at the database boundary.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum DbError {
    #[error("authentication failed: {0}")]
    AuthFailed(String),
    #[error("connection failed: {0}")]
    ConnectionFailed(String),
    #[error("TLS configuration error: {0}")]
    TlsConfig(String),
    #[error("{message}")]
    QueryFailed {
        message: String,
        code: Option<String>,
        detail: Option<String>,
        hint: Option<String>,
    },
    #[error("database session is disconnected")]
    Disconnected,
    #[error("query cancelled by user")]
    Cancelled,
    #[error("invalid input: {0}")]
    InvalidInput(String),
}

impl DbError {
    /// Returns true if the failure indicates credentials were rejected or missing.
    pub fn is_auth_failure(&self) -> bool {
        matches!(self, Self::AuthFailed(_))
    }
}

/// Result of executing one individual SQL statement in the SQL query editor.
#[derive(Clone, Debug, PartialEq)]
pub struct QueryStatementResult {
    pub sql: String,
    pub columns: Vec<ColumnMeta>,
    pub rows: Vec<Vec<CellValue>>,
    pub rows_affected: Option<u64>,
    pub truncated: bool,
    pub total_rows_seen: usize,
    pub elapsed_ms: u64,
    pub error: Option<String>,
}

impl QueryStatementResult {
    pub fn is_tabular(&self) -> bool {
        !self.columns.is_empty()
    }

    pub fn summary_label(&self) -> String {
        if let Some(err) = &self.error {
            return format!("Error: {err}");
        }
        if self.is_tabular() {
            if self.truncated {
                format!(
                    "{} of {} rows (limited) · {} ms",
                    self.rows.len(),
                    self.total_rows_seen,
                    self.elapsed_ms
                )
            } else {
                format!("{} rows · {} ms", self.rows.len(), self.elapsed_ms)
            }
        } else if let Some(n) = self.rows_affected {
            format!("{n} rows affected · {} ms", self.elapsed_ms)
        } else {
            format!("Completed · {} ms", self.elapsed_ms)
        }
    }
}

/// Complete outcome of executing one or more statements from the SQL query editor.
#[derive(Clone, Debug, PartialEq)]
pub struct QueryBatchResult {
    pub results: Vec<QueryStatementResult>,
    pub total_elapsed_ms: u64,
    pub cancelled: bool,
}

/// Record of an introspection or user query sent to the database, surfaced in the console log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutedSql {
    pub sql: String,
    pub duration_ms: u64,
    pub error: Option<String>,
}

/// Outcome reported after committing staged table edits inside one SQL transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransactionOutcome {
    pub statements_executed: usize,
    pub total_rows_affected: u64,
    pub elapsed_ms: u64,
}

/// Referenced target row fetched when peeking a foreign-key cell value.
#[derive(Clone, Debug, PartialEq)]
pub struct FkPeekRow {
    pub fk: ForeignKeyRef,
    pub lookup_display: String,
    pub columns: Vec<ColumnMeta>,
    pub row: Option<Vec<CellValue>>,
    pub elapsed_ms: u64,
}

/// Engine-agnostic database session contract used by the workspace worker.
#[async_trait]
pub trait DbSession: Send + Sync {
    fn server_info(&self) -> &ServerInfo;
    async fn list_databases(&self) -> Result<(Vec<DatabaseInfo>, ExecutedSql), DbError>;
    async fn list_schemas(&self) -> Result<(Vec<SchemaInfo>, ExecutedSql), DbError>;
    async fn list_objects(&self, schema: &str) -> Result<(Vec<DbObject>, ExecutedSql), DbError>;
    async fn list_columns(&self, schema: &str, object: &str) -> Result<(Vec<ColumnSummary>, ExecutedSql), DbError>;
    async fn fetch_table_page(&self, req: &PageRequest) -> Result<(TablePage, Vec<ExecutedSql>), DbError>;
    async fn describe_table(&self, schema: &str, table: &str) -> Result<(TableStructure, Vec<ExecutedSql>), DbError>;
    async fn execute_sql(
        &self,
        statements: &[String],
        row_limit: Option<usize>,
    ) -> Result<(QueryBatchResult, Vec<ExecutedSql>), DbError>;
    async fn execute_transaction(
        &self,
        statements: &[String],
    ) -> Result<(TransactionOutcome, Vec<ExecutedSql>), DbError>;
    async fn explain_sql(&self, statement: &str, analyze: bool) -> Result<(ExplainPlan, ExecutedSql), DbError>;
    async fn fetch_fk_row(
        &self,
        fk: &ForeignKeyRef,
        value: &CellValue,
        source_col: &ColumnMeta,
    ) -> Result<(FkPeekRow, ExecutedSql), DbError>;
    async fn cancel_running_query(&self) -> Result<(), DbError>;
    async fn ping(&self) -> Result<(), DbError>;
}
