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
    AccentColor, ConnectionProfile, DbEngine, EnvTag, PasswordMode, SafetyPolicy, SslConfig, SslMode, UrlParseError,
    ValidationError,
};
pub use engine::{
    CellValue, ColumnMeta, ColumnSummary, DatabaseInfo, DbError, DbObject, DbObjectKind, DbSession, ExecutedSql,
    FkPeekRow, ForeignKeyRef, IndexInfo, MysqlRawValue, PageRequest, PostgresRawValue, QueryBatchResult,
    QueryStatementResult, RawCellValue, SchemaInfo, ServerInfo, SortSpec, SqliteRawValue, TablePage, TableStats,
    TableStructure, TableTransactionSupport, TransactionOutcome, connect_session, test_connection,
};
pub use explain::{ExplainError, ExplainNode, ExplainPlan, build_explain_sql, build_explain_sql_for_engine};
pub use export::{
    ExportFormat, default_export_filename, export_csv, export_json, export_rows, export_rows_for_engine,
    export_sql_inserts, export_sql_inserts_for_engine,
};
pub use history::{NewHistoryEntry, QueryHistoryEntry, QueryStore, QueryStoreError, SavedQuery};
pub use ids::{ConnectionId, RequestId, SavedQueryId, SessionId, TabId};
pub use safety::{
    SafetyDecision, StatementInspection, StatementRisk, evaluate_sql_safety, evaluate_sql_safety_for_engine,
    evaluate_sql_safety_for_engine_with_mysql_mode, evaluate_sql_safety_for_engine_with_session_mode,
    evaluate_staged_commit_safety, inspect_statement, inspect_statement_for_engine,
    inspect_statement_for_engine_with_mysql_mode, inspect_statement_for_engine_with_session_mode,
};
pub use sql::{
    MysqlFlavor, MysqlQuoteMode, MysqlServerVersion, PostgresSavepointMode, PostgresServerEncoding,
    PostgresSessionMode, PostgresTxState, SqlSessionMode, SqlStatement,
    batch_sets_credentials_with_mysql_mode, batch_sets_credentials_with_session_mode, cursor_line_col,
    format_sql, has_executable_sql, has_executable_sql_for_engine, mysql_slice_has_multiple_statements,
    postgres_slice_has_multiple_statements, split_statements, split_statements_for_engine,
    split_statements_for_engine_with_mysql_mode, split_statements_for_engine_with_session_mode, sql_sets_credentials,
    sql_sets_credentials_with_mysql_mode, sql_sets_credentials_with_session_mode, statement_at_cursor,
    statement_at_cursor_for_engine, statement_at_cursor_for_engine_with_mysql_mode,
    statement_at_cursor_for_engine_with_session_mode, trim_sql_whitespace_for_engine,
    validate_mysql_single_statement_batch,
};
pub use staging::{
    DraftRowId, StagedCellValue, StagedChanges, StagedColumnAssignment, StagedColumnValue, StagedCommitBatch,
    StagedInsertRow, StagedMutation, StagedRowDelete, StagedRowUpdate, StagingError, build_fk_where_clause,
    build_fk_where_clause_for_engine, escape_mysql_string, escape_sql_string, format_cell_literal, format_literal,
    format_mysql_string_literal, format_pg_cell_literal, format_pg_literal, validate_row_editable,
};
pub use storage::{
    AppSettings, AppearancePreference, ConnectionStore, SETTINGS_SCHEMA_VERSION, SettingsStore, SortOrder, StorageError,
    StoreData, atomic_write_bytes,
};
