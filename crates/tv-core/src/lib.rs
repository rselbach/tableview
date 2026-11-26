#![forbid(unsafe_code)]

pub mod connection;
pub mod engine;
pub mod history;
pub mod ids;
pub mod sql;
pub mod storage;

pub use connection::{
    AccentColor, ConnectionProfile, EnvTag, PasswordMode, SafetyPolicy, SslConfig, SslMode, UrlParseError,
    ValidationError,
};
pub use engine::{
    CellValue, ColumnMeta, ColumnSummary, DatabaseInfo, DbError, DbObject, DbObjectKind, DbSession, ExecutedSql,
    ForeignKeyRef, IndexInfo, PageRequest, QueryBatchResult, QueryStatementResult, SchemaInfo, ServerInfo, SortSpec,
    TablePage, TableStats, TableStructure,
};
pub use history::{NewHistoryEntry, QueryHistoryEntry, QueryStore, QueryStoreError, SavedQuery};
pub use ids::{ConnectionId, RequestId, SavedQueryId, SessionId, TabId};
pub use sql::{SqlStatement, cursor_line_col, format_sql, split_statements, statement_at_cursor};
pub use storage::{ConnectionStore, SortOrder, StorageError, StoreData};
