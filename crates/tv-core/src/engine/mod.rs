pub mod mysql;
pub mod postgres;
pub mod sqlite;

use std::sync::Arc;

use crate::connection::{ConnectionProfile, DbEngine};
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
    pub source_columns: Vec<String>,
    pub target_columns: Vec<String>,
}

impl ForeignKeyRef {
    /// Constructs a single-column foreign-key reference.
    pub fn single(
        constraint_name: impl Into<String>,
        source_column: impl Into<String>,
        target_schema: impl Into<String>,
        target_table: impl Into<String>,
        target_column: impl Into<String>,
    ) -> Self {
        Self {
            constraint_name: constraint_name.into(),
            target_schema: target_schema.into(),
            target_table: target_table.into(),
            source_columns: vec![source_column.into()],
            target_columns: vec![target_column.into()],
        }
    }

    /// Returns `true` when the target relation and full ordered column mapping are known.
    pub fn is_resolvable(&self) -> bool {
        !self.target_table.trim().is_empty()
            && !self.source_columns.is_empty()
            && self.source_columns.len() == self.target_columns.len()
            && self.source_columns.iter().all(|c| !c.trim().is_empty())
            && self.target_columns.iter().all(|c| !c.trim().is_empty())
    }

    /// Returns `true` when this constraint references more than one column.
    pub fn is_composite(&self) -> bool {
        self.source_columns.len() > 1
    }

    /// Returns the single target column name when this constraint is single-column and resolvable.
    pub fn target_column(&self) -> Option<&str> {
        (self.target_columns.len() == 1 && self.is_resolvable()).then(|| self.target_columns[0].as_str())
    }

    /// Returns the target column paired with `source_column` when this constraint is resolvable.
    pub fn target_column_for(&self, source_column: &str) -> Option<&str> {
        if !self.is_resolvable() {
            return None;
        }
        self.source_columns
            .iter()
            .zip(self.target_columns.iter())
            .find(|(src, _)| src.as_str() == source_column)
            .map(|(_, dst)| dst.as_str())
    }
}

/// Ordered column value binding used to peek or jump to a referenced foreign-key row.
#[derive(Clone, Debug, PartialEq)]
pub struct FkLookupComponent {
    pub target_column: String,
    pub source_col: ColumnMeta,
    pub value: CellValue,
    pub raw_value: Option<RawCellValue>,
}

/// Deterministic key or system row identifier used to stabilize paginated table ordering.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StableOrderKey {
    Column {
        name: String,
        nullable: bool,
        collation: Option<String>,
    },
    System(&'static str),
}

pub(crate) struct DecodedPageBatch {
    pub rows: Vec<Vec<CellValue>>,
    pub raw_rows: Vec<Vec<RawCellValue>>,
    pub has_more: bool,
    pub log: ExecutedSql,
}

pub(crate) struct TableMetaWithOrder {
    pub columns: Vec<ColumnMeta>,
    pub foreign_keys: Vec<ForeignKeyRef>,
    pub stable_order: Vec<StableOrderKey>,
    pub log: ExecutedSql,
}

pub(crate) fn last_page_offset(total_rows: i64, limit: i64) -> i64 {
    if total_rows <= 0 || limit <= 0 {
        0
    } else {
        ((total_rows - 1) / limit) * limit
    }
}

pub(crate) fn attach_foreign_keys_to_columns(
    constraints: Vec<ForeignKeyRef>,
) -> std::collections::HashMap<String, ForeignKeyRef> {
    use std::cmp::Reverse;

    let mut attached: std::collections::HashMap<String, ForeignKeyRef> = std::collections::HashMap::new();
    for fk in constraints {
        for src in &fk.source_columns {
            if src.trim().is_empty() {
                continue;
            }
            let replace = match attached.get(src) {
                None => true,
                Some(existing) => {
                    (
                        fk.is_resolvable(),
                        Reverse(fk.source_columns.len()),
                        Reverse(fk.constraint_name.as_str()),
                    ) > (
                        existing.is_resolvable(),
                        Reverse(existing.source_columns.len()),
                        Reverse(existing.constraint_name.as_str()),
                    )
                }
            };
            if replace {
                attached.insert(src.clone(), fk.clone());
            }
        }
    }
    attached
}

/// Returns all unique foreign-key constraints from `explicit` followed by any fallback
/// `ColumnMeta.foreign_key` entries not already represented.
pub fn collect_foreign_keys<'a>(
    explicit: &'a [ForeignKeyRef],
    columns: &'a [ColumnMeta],
) -> Vec<&'a ForeignKeyRef> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for fk in explicit.iter().chain(columns.iter().filter_map(|c| c.foreign_key.as_ref())) {
        if seen.insert((
            fk.constraint_name.as_str(),
            fk.target_schema.as_str(),
            fk.target_table.as_str(),
            fk.source_columns.as_slice(),
            fk.target_columns.as_slice(),
        )) {
            out.push(fk);
        }
    }
    out
}

/// Returns all unique foreign-key constraints referencing `column`, placing `ColumnMeta.foreign_key` first if present.
pub fn foreign_keys_for_column<'a>(
    explicit: &'a [ForeignKeyRef],
    columns: &'a [ColumnMeta],
    column: &str,
) -> Vec<&'a ForeignKeyRef> {
    let mut matching: Vec<&'a ForeignKeyRef> = collect_foreign_keys(explicit, columns)
        .into_iter()
        .filter(|fk| fk.source_columns.iter().any(|src| src == column))
        .collect();
    if let Some(primary) = columns
        .iter()
        .find(|c| c.name == column)
        .and_then(|c| c.foreign_key.as_ref())
        && let Some(pos) = matching.iter().position(|fk| *fk == primary)
        && pos > 0
    {
        let primary_ref = matching.remove(pos);
        matching.insert(0, primary_ref);
    }
    matching
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

/// Relation size estimates, transaction capability, comment, and synthesized DDL for a database object.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum TableTransactionSupport {
    #[default]
    Supported,
    Unsupported {
        engine: String,
    },
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableStats {
    pub total_bytes: Option<i64>,
    pub data_bytes: Option<i64>,
    pub index_bytes: Option<i64>,
    pub estimated_rows: Option<i64>,
    pub comment: Option<String>,
    pub ddl: String,
    pub transaction_support: TableTransactionSupport,
}

/// Complete structural inspection payload for the Structure and Indexes views and inspector details.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableStructure {
    pub columns: Vec<ColumnMeta>,
    pub foreign_keys: Vec<ForeignKeyRef>,
    pub indexes: Vec<IndexInfo>,
    pub stats: TableStats,
}

impl TableStructure {
    /// Returns all unique foreign-key constraints declared on this table.
    pub fn all_foreign_keys(&self) -> Vec<&ForeignKeyRef> {
        collect_foreign_keys(&self.foreign_keys, &self.columns)
    }

    /// Returns all unique foreign-key constraints referencing `column`.
    pub fn foreign_keys_for_column(&self, column: &str) -> Vec<&ForeignKeyRef> {
        foreign_keys_for_column(&self.foreign_keys, &self.columns, column)
    }
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

/// Raw wire/storage value preserved per engine so staged writes bind exact identity & concurrency tokens.
#[derive(Clone, Debug, PartialEq)]
pub enum SqliteRawValue {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    NonUtf8Text(Vec<u8>),
    Blob(Vec<u8>),
}

impl SqliteRawValue {
    pub const fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    pub const fn typeof_name(&self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Integer(_) => "integer",
            Self::Real(_) => "real",
            Self::Text(_) | Self::NonUtf8Text(_) => "text",
            Self::Blob(_) => "blob",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PostgresRawValue {
    Null,
    Text(String),
    Bytes(Vec<u8>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum MysqlRawValue {
    Null,
    Int(i64),
    UInt(u64),
    Float(f32),
    Double(f64),
    Bytes(Vec<u8>),
    Date {
        year: u16,
        month: u8,
        day: u8,
        hour: u8,
        minute: u8,
        second: u8,
        micros: u32,
    },
    Time {
        neg: bool,
        days: u32,
        hour: u8,
        minute: u8,
        second: u8,
        micros: u32,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum RawCellValue {
    Sqlite(SqliteRawValue),
    Postgres(PostgresRawValue),
    Mysql(MysqlRawValue),
}

impl RawCellValue {
    pub const fn is_null(&self) -> bool {
        match self {
            Self::Sqlite(v) => v.is_null(),
            Self::Postgres(v) => matches!(v, PostgresRawValue::Null),
            Self::Mysql(v) => matches!(v, MysqlRawValue::Null),
        }
    }
}

/// One page of rows decoded into typed [`CellValue`]s alongside raw snapshots, columns, and timing.
#[derive(Clone, Debug, PartialEq)]
pub struct TablePage {
    pub columns: Vec<ColumnMeta>,
    pub foreign_keys: Vec<ForeignKeyRef>,
    pub rows: Vec<Vec<CellValue>>,
    pub raw_rows: Vec<Vec<RawCellValue>>,
    pub transaction_support: TableTransactionSupport,
    pub offset: i64,
    pub limit: i64,
    pub total_rows_estimate: Option<i64>,
    pub total_is_exact: bool,
    pub has_more: bool,
    pub elapsed_ms: u64,
}

impl TablePage {
    pub fn raw_row(&self, idx: usize) -> Option<&[RawCellValue]> {
        self.raw_rows.get(idx).map(Vec::as_slice)
    }

    /// Returns all unique foreign-key constraints declared on this table.
    pub fn all_foreign_keys(&self) -> Vec<&ForeignKeyRef> {
        collect_foreign_keys(&self.foreign_keys, &self.columns)
    }

    /// Returns all unique foreign-key constraints referencing `column`.
    pub fn foreign_keys_for_column(&self, column: &str) -> Vec<&ForeignKeyRef> {
        foreign_keys_for_column(&self.foreign_keys, &self.columns, column)
    }
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

pub(crate) fn decode_hex(raw: &str) -> Option<Vec<u8>> {
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
    #[error("database session disconnected before dispatch: {0}")]
    DisconnectedBeforeDispatch(Box<DbError>),
    #[error("query cancelled by user")]
    Cancelled,
    #[error("MySQL session quote mode or server version changed before dispatch; re-run to inspect under the current mode")]
    MysqlModeMismatch,
    #[error("PostgreSQL string escaping mode changed before dispatch; re-run to inspect under the current mode")]
    PostgresStringModeMismatch,
    #[error("{0}")]
    StaleRowConflict(String),
    #[error("{message}")]
    NonTransactionalTable {
        support: TableTransactionSupport,
        message: String,
    },
    #[error("invalid input: {0}")]
    InvalidInput(String),
}

impl DbError {
    /// Returns true if the failure indicates credentials were rejected or missing.
    pub fn is_auth_failure(&self) -> bool {
        match self {
            Self::AuthFailed(_) => true,
            Self::DisconnectedBeforeDispatch(inner) => inner.is_auth_failure(),
            _ => false,
        }
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
    fn engine(&self) -> DbEngine;
    fn server_info(&self) -> &ServerInfo;
    /// Returns live MySQL session quoting flags when backed by a MySQL/MariaDB session.
    fn mysql_quote_mode(&self) -> Option<crate::sql::MysqlQuoteMode> {
        None
    }
    /// Returns live MySQL quoting flags for pooled grid connections (`@@GLOBAL.sql_mode`),
    /// defaulting to `mysql_quote_mode()` for non-MySQL or mock sessions.
    fn mysql_grid_quote_mode(&self) -> Option<crate::sql::MysqlQuoteMode> {
        self.mysql_quote_mode()
    }
    /// Returns live PostgreSQL session string-escaping and transaction state when backed by PostgreSQL.
    fn postgres_session_mode(&self) -> Option<crate::sql::PostgresSessionMode> {
        None
    }
    /// Returns live PostgreSQL `standard_conforming_strings` state when backed by a PostgreSQL session.
    fn postgres_standard_conforming_strings(&self) -> Option<bool> {
        self.postgres_session_mode()
            .map(|mode| mode.standard_conforming_strings)
    }
    /// Returns `true` when the underlying transport is already known to be closed.
    fn is_disconnected(&self) -> bool {
        false
    }
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
    async fn execute_sql_cancellable(
        &self,
        statements: &[String],
        row_limit: Option<usize>,
        _cancel_key: u64,
        cancel_flag: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<(QueryBatchResult, Vec<ExecutedSql>), DbError> {
        if cancel_flag.is_some_and(|f| f.load(std::sync::atomic::Ordering::Acquire)) {
            return Err(DbError::Cancelled);
        }
        self.execute_sql(statements, row_limit).await
    }
    /// Executes SQL after verifying that any live MySQL quote mode/version or PostgreSQL
    /// `standard_conforming_strings` setting matches `expected_sql_mode` on the dispatch connection.
    async fn execute_sql_cancellable_checked(
        &self,
        statements: &[String],
        row_limit: Option<usize>,
        cancel_key: u64,
        cancel_flag: Option<&std::sync::atomic::AtomicBool>,
        expected_sql_mode: crate::sql::SqlSessionMode,
    ) -> Result<(QueryBatchResult, Vec<ExecutedSql>), DbError> {
        match self.engine() {
            DbEngine::Mysql => {
                let live_mode = self.mysql_quote_mode();
                if live_mode != expected_sql_mode.mysql {
                    return Err(DbError::MysqlModeMismatch);
                }
                if let Err(msg) = crate::sql::validate_mysql_single_statement_batch(statements, live_mode) {
                    return Err(DbError::InvalidInput(msg));
                }
            }
            DbEngine::Postgres => {
                if let Some(ref expected_pg) = expected_sql_mode.postgres {
                    if self.postgres_session_mode().as_ref() != Some(expected_pg) {
                        return Err(DbError::PostgresStringModeMismatch);
                    }
                    let mut tracker = crate::sql::PgScsTracker::from_mode(expected_pg);
                    for raw_stmt in statements {
                        let stmt = crate::sql::trim_sql_whitespace_for_engine(DbEngine::Postgres, raw_stmt);
                        if stmt.is_empty() {
                            continue;
                        }
                        let is_multi = match tracker.active {
                            Some(scs) => crate::sql::postgres_slice_has_multiple_statements(stmt, scs),
                            None => {
                                crate::sql::postgres_slice_has_multiple_statements(stmt, true)
                                    || crate::sql::postgres_slice_has_multiple_statements(stmt, false)
                            }
                        };
                        if is_multi {
                            return Err(DbError::InvalidInput(
                                "PostgreSQL statement contains multiple semicolon-separated statements; split statements before executing"
                                    .to_owned(),
                            ));
                        }
                        tracker.apply(stmt);
                    }
                }
            }
            DbEngine::Sqlite => {}
        }
        self.execute_sql_cancellable(statements, row_limit, cancel_key, cancel_flag)
            .await
    }
    async fn execute_transaction(
        &self,
        statements: &[String],
    ) -> Result<(TransactionOutcome, Vec<ExecutedSql>), DbError>;
    async fn execute_staged_mutations(
        &self,
        batch: &crate::staging::StagedCommitBatch,
    ) -> Result<(TransactionOutcome, Vec<ExecutedSql>), DbError>;
    async fn explain_sql(&self, statement: &str, analyze: bool) -> Result<(ExplainPlan, ExecutedSql), DbError>;
    /// Explains SQL after verifying that any live MySQL quote mode/version or PostgreSQL
    /// `standard_conforming_strings` setting matches `expected_sql_mode` on the dispatch connection.
    async fn explain_sql_checked(
        &self,
        statement: &str,
        analyze: bool,
        expected_sql_mode: crate::sql::SqlSessionMode,
    ) -> Result<(ExplainPlan, ExecutedSql), DbError> {
        match self.engine() {
            DbEngine::Mysql => {
                let live_mode = self.mysql_quote_mode();
                if live_mode != expected_sql_mode.mysql {
                    return Err(DbError::MysqlModeMismatch);
                }
                if crate::sql::mysql_slice_has_multiple_statements(
                    crate::sql::trim_sql_whitespace_for_engine(DbEngine::Mysql, statement),
                    live_mode,
                ) {
                    return Err(DbError::InvalidInput(
                        "cannot EXPLAIN a MySQL slice containing multiple statements; select or split a single statement"
                            .to_owned(),
                    ));
                }
            }
            DbEngine::Postgres => {
                if let Some(ref expected_pg) = expected_sql_mode.postgres {
                    if self.postgres_session_mode().as_ref() != Some(expected_pg) {
                        return Err(DbError::PostgresStringModeMismatch);
                    }
                    if crate::sql::postgres_slice_has_multiple_statements(
                        crate::sql::trim_sql_whitespace_for_engine(DbEngine::Postgres, statement),
                        expected_pg.standard_conforming_strings,
                    ) {
                        return Err(DbError::InvalidInput(
                            "cannot EXPLAIN a PostgreSQL slice containing multiple statements; select or split a single statement"
                                .to_owned(),
                        ));
                    }
                }
            }
            DbEngine::Sqlite => {}
        }
        self.explain_sql(statement, analyze).await
    }
    async fn fetch_fk_row(
        &self,
        fk: &ForeignKeyRef,
        components: &[FkLookupComponent],
    ) -> Result<(FkPeekRow, ExecutedSql), DbError>;
    async fn cancel_running_query(&self) -> Result<(), DbError>;
    async fn cancel_query_by_key(&self, _cancel_key: u64) -> Result<(), DbError> {
        self.cancel_running_query().await
    }
    async fn ping(&self) -> Result<(), DbError>;
}

/// Connects to the engine configured in `profile` and returns an active [`DbSession`].
pub async fn connect_session(
    profile: &ConnectionProfile,
    password: Option<&str>,
    database_override: Option<&str>,
) -> Result<(Arc<dyn DbSession>, Vec<ExecutedSql>), DbError> {
    match profile.engine {
        DbEngine::Postgres => {
            let (session, logs) = postgres::PostgresSession::connect(profile, password, database_override).await?;
            Ok((Arc::new(session), logs))
        }
        DbEngine::Mysql => {
            let (session, logs) = mysql::MysqlSession::connect(profile, password, database_override).await?;
            Ok((Arc::new(session), logs))
        }
        DbEngine::Sqlite => {
            let (session, logs) = sqlite::SqliteSession::connect(profile, password, database_override).await?;
            Ok((Arc::new(session), logs))
        }
    }
}

/// Tests connectivity against the engine configured in `profile` without retaining a session.
pub async fn test_connection(profile: &ConnectionProfile, password: Option<&str>) -> Result<ServerInfo, DbError> {
    match profile.engine {
        DbEngine::Postgres => postgres::PostgresSession::test_connection(profile, password).await,
        DbEngine::Mysql => mysql::MysqlSession::test_connection(profile, password).await,
        DbEngine::Sqlite => sqlite::SqliteSession::test_connection(profile, password).await,
    }
}
