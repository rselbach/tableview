#![forbid(unsafe_code)]

pub mod connection;
pub mod engine;
pub mod explain;
pub mod export;
pub mod history;
pub mod ids;
pub mod safety;
pub mod sql;
pub mod staging;
pub mod storage;

pub use connection::{
    AccentColor, ConnectionProfile, EnvTag, PasswordMode, SafetyPolicy, SslConfig, SslMode, UrlParseError,
    ValidationError,
};
pub use engine::{
    CellValue, ColumnMeta, ColumnSummary, DatabaseInfo, DbError, DbObject, DbObjectKind, DbSession, ExecutedSql,
    FkPeekRow, ForeignKeyRef, IndexInfo, PageRequest, QueryBatchResult, QueryStatementResult, SchemaInfo, ServerInfo,
    SortSpec, TablePage, TableStats, TableStructure, TransactionOutcome,
};
pub use explain::{ExplainError, ExplainNode, ExplainPlan, build_explain_sql};
pub use export::{ExportFormat, default_export_filename, export_csv, export_json, export_rows, export_sql_inserts};
pub use history::{NewHistoryEntry, QueryHistoryEntry, QueryStore, QueryStoreError, SavedQuery};
pub use ids::{ConnectionId, RequestId, SavedQueryId, SessionId, TabId};
pub use safety::{SafetyDecision, StatementInspection, StatementRisk, evaluate_sql_safety, evaluate_staged_commit_safety, inspect_statement};
pub use sql::{SqlStatement, cursor_line_col, format_sql, split_statements, statement_at_cursor};
pub use staging::{
    DraftRowId, StagedCellValue, StagedChanges, StagedInsertRow, StagedRowUpdate, StagingError, build_fk_where_clause,
    escape_sql_string, format_pg_cell_literal, format_pg_literal,
};
pub use storage::{
    AppSettings, AppearancePreference, ConnectionStore, SETTINGS_SCHEMA_VERSION, SettingsStore, SortOrder, StorageError,
    StoreData,
};
