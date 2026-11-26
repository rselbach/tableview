#![forbid(unsafe_code)]

pub mod connection;
pub mod engine;
pub mod ids;
pub mod storage;

pub use connection::{
    AccentColor, ConnectionProfile, EnvTag, PasswordMode, SafetyPolicy, SslConfig, SslMode, UrlParseError,
    ValidationError,
};
pub use engine::{
    CellValue, ColumnMeta, ColumnSummary, DatabaseInfo, DbError, DbObject, DbObjectKind, DbSession, ExecutedSql,
    ForeignKeyRef, IndexInfo, PageRequest, SchemaInfo, ServerInfo, SortSpec, TablePage, TableStats, TableStructure,
};
pub use ids::{ConnectionId, RequestId, SessionId, TabId};
pub use storage::{ConnectionStore, SortOrder, StorageError, StoreData};
