pub mod postgres;

use async_trait::async_trait;
use thiserror::Error;

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
    #[error("invalid input: {0}")]
    InvalidInput(String),
}

impl DbError {
    /// Returns true if the failure indicates credentials were rejected or missing.
    pub fn is_auth_failure(&self) -> bool {
        matches!(self, Self::AuthFailed(_))
    }
}

/// Record of an introspection or user query sent to the database, surfaced in the console log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutedSql {
    pub sql: String,
    pub duration_ms: u64,
    pub error: Option<String>,
}

/// Engine-agnostic database session contract used by the workspace worker.
#[async_trait]
pub trait DbSession: Send + Sync {
    fn server_info(&self) -> &ServerInfo;
    async fn list_databases(&self) -> Result<(Vec<DatabaseInfo>, ExecutedSql), DbError>;
    async fn list_schemas(&self) -> Result<(Vec<SchemaInfo>, ExecutedSql), DbError>;
    async fn list_objects(&self, schema: &str) -> Result<(Vec<DbObject>, ExecutedSql), DbError>;
    async fn list_columns(&self, schema: &str, object: &str) -> Result<(Vec<ColumnSummary>, ExecutedSql), DbError>;
    async fn ping(&self) -> Result<(), DbError>;
}
