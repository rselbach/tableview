use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rusqlite::types::{Value as SqliteValue, ValueRef};
use rusqlite::{Connection, ErrorCode, InterruptHandle, OpenFlags, params_from_iter};

use crate::connection::{ConnectionProfile, DbEngine, SafetyPolicy};
use crate::engine::{
    CellValue, ColumnMeta, ColumnSummary, DatabaseInfo, DbError, DbObject, DbObjectKind, DbSession, ExecutedSql,
    FkPeekRow, ForeignKeyRef, IndexInfo, PageRequest, QueryBatchResult, QueryStatementResult, RawCellValue, SchemaInfo,
    ServerInfo, SqliteRawValue, TablePage, TableStats, TableStructure, TableTransactionSupport, TransactionOutcome,
    decode_hex,
};
use crate::explain::{ExplainPlan, build_explain_sql_for_engine};
use crate::staging::{
    LogicalColType, StagedCellValue, StagedColumnValue, StagedCommitBatch, StagedMutation, build_fk_where_clause_for_engine,
    classify_column_type, strip_hex_prefix,
};

#[derive(Default)]
struct SqliteCancelState {
    active_key: Option<u64>,
    cancel_requested: bool,
    waiters: HashMap<u64, tokio::sync::oneshot::Sender<()>>,
    cancelled_keys: HashSet<u64>,
}

struct SqliteWaiterGuard {
    cancel_state: Arc<Mutex<SqliteCancelState>>,
    cancel_key: u64,
}

impl Drop for SqliteWaiterGuard {
    fn drop(&mut self) {
        if let Ok(mut state) = self.cancel_state.lock() {
            state.waiters.remove(&self.cancel_key);
            state.cancelled_keys.remove(&self.cancel_key);
        }
    }
}

struct SqliteActiveKeyGuard {
    cancel_state: Arc<Mutex<SqliteCancelState>>,
    cancel_key: Option<u64>,
}

impl Drop for SqliteActiveKeyGuard {
    fn drop(&mut self) {
        if let Ok(mut state) = self.cancel_state.lock()
            && state.active_key == self.cancel_key
        {
            state.active_key = None;
            state.cancel_requested = false;
        }
    }
}

fn flag_cancelled(cancel_flag: Option<&AtomicBool>) -> bool {
    cancel_flag.is_some_and(|flag| flag.load(Ordering::Acquire))
}

fn is_readonly_sqlite_pragma(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
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

/// Active SQLite database session backed by `rusqlite` on Tokio's blocking pool.
pub struct SqliteSession {
    conn: Arc<Mutex<Connection>>,
    conn_gate: Arc<tokio::sync::Mutex<()>>,
    cancel_state: Arc<Mutex<SqliteCancelState>>,
    interrupt_handle: InterruptHandle,
    server_info: ServerInfo,
    read_only: bool,
    file_path: Option<PathBuf>,
}

impl SqliteSession {
    /// Opens a SQLite database file or `:memory:` database according to `profile`.
    pub async fn connect(
        profile: &ConnectionProfile,
        _password: Option<&str>,
        _database_override: Option<&str>,
    ) -> Result<(Self, Vec<ExecutedSql>), DbError> {
        profile
            .validate()
            .map_err(|e| DbError::InvalidInput(e.to_string()))?;

        let raw_path = profile
            .file_path
            .as_ref()
            .ok_or_else(|| DbError::InvalidInput("missing SQLite database file path".to_owned()))?
            .clone();
        let is_memory = is_in_memory_path(&raw_path);
        let read_only = profile.safety == SafetyPolicy::ReadOnly;

        if !is_memory && !raw_path.exists() {
            return Err(DbError::ConnectionFailed(format!(
                "SQLite database file does not exist: {}",
                raw_path.display()
            )));
        }

        let current_database = if is_memory {
            ":memory:".to_owned()
        } else {
            raw_path
                .file_name()
                .and_then(|s| s.to_str())
                .map(str::to_owned)
                .unwrap_or_else(|| raw_path.display().to_string())
        };

        let path_for_open = raw_path.clone();
        let (conn, interrupt_handle, version, version_log) = tokio::task::spawn_blocking(move || {
            let mut conn = if is_memory {
                Connection::open_in_memory().map_err(|e| DbError::ConnectionFailed(e.to_string()))?
            } else {
                let flags = if read_only {
                    OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX
                } else {
                    OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX
                };
                Connection::open_with_flags(&path_for_open, flags)
                    .map_err(|e| DbError::ConnectionFailed(e.to_string()))?
            };

            conn.execute_batch("PRAGMA busy_timeout = 5000; PRAGMA foreign_keys = ON;")
                .map_err(|e| DbError::ConnectionFailed(e.to_string()))?;
            if read_only {
                conn.execute_batch("PRAGMA query_only = ON;")
                    .map_err(|e| DbError::ConnectionFailed(e.to_string()))?;
                conn.authorizer(Some(|ctx: rusqlite::hooks::AuthContext<'_>| {
                    use rusqlite::hooks::{AuthAction, Authorization};
                    match ctx.action {
                        AuthAction::Select
                        | AuthAction::Read { .. }
                        | AuthAction::Function { .. }
                        | AuthAction::Recursive => Authorization::Allow,
                        AuthAction::Pragma {
                            pragma_name,
                            pragma_value,
                        } if pragma_value.is_none() || is_readonly_sqlite_pragma(pragma_name) => {
                            Authorization::Allow
                        }
                        _ => Authorization::Deny,
                    }
                }))
                .map_err(|e| DbError::ConnectionFailed(e.to_string()))?;
            }

            let sql = "SELECT sqlite_version()";
            let start = Instant::now();
            let ver: String = conn
                .query_row(sql, [], |row| row.get(0))
                .map_err(|e| DbError::ConnectionFailed(e.to_string()))?;
            let log = ExecutedSql {
                sql: sql.to_owned(),
                duration_ms: start.elapsed().as_millis() as u64,
                error: None,
            };
            let interrupt_handle = conn.get_interrupt_handle();
            Ok::<_, DbError>((conn, interrupt_handle, ver, log))
        })
        .await
        .map_err(|e| DbError::ConnectionFailed(format!("SQLite connect task failed: {e}")))??;

        let session = Self {
            conn: Arc::new(Mutex::new(conn)),
            conn_gate: Arc::new(tokio::sync::Mutex::new(())),
            cancel_state: Arc::new(Mutex::new(SqliteCancelState::default())),
            interrupt_handle,
            server_info: ServerInfo {
                version: format!("SQLite {version}"),
                current_database,
                current_user: String::new(),
            },
            read_only,
            file_path: (!is_memory).then_some(raw_path),
        };
        Ok((session, vec![version_log]))
    }

    /// Tests connectivity to a SQLite file or `:memory:` database without retaining a session.
    pub async fn test_connection(
        profile: &ConnectionProfile,
        password: Option<&str>,
    ) -> Result<ServerInfo, DbError> {
        let (session, _) = Self::connect(profile, password, None).await?;
        Ok(session.server_info().clone())
    }

    async fn with_conn<F, R>(&self, f: F) -> Result<R, DbError>
    where
        F: FnOnce(&mut Connection) -> Result<R, DbError> + Send + 'static,
        R: Send + 'static,
    {
        let gate = Arc::clone(&self.conn_gate).lock_owned().await;
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let _gate = gate;
            let mut guard = conn.lock().map_err(|_| DbError::Disconnected)?;
            f(&mut guard)
        })
        .await
        .map_err(|e| DbError::ConnectionFailed(format!("SQLite worker task failed: {e}")))?
    }

    async fn execute_sql_inner(
        &self,
        statements: &[String],
        row_limit: Option<usize>,
        cancel_key: Option<u64>,
        cancel_flag: Option<&AtomicBool>,
    ) -> Result<(QueryBatchResult, Vec<ExecutedSql>), DbError> {
        let (gate, _waiter_guard) = if let Some(key) = cancel_key {
            if flag_cancelled(cancel_flag) {
                return Err(DbError::Cancelled);
            }
            let (cancel_tx, mut cancel_rx) = tokio::sync::oneshot::channel();
            {
                let mut state = self.cancel_state.lock().map_err(|_| DbError::Disconnected)?;
                if state.cancelled_keys.remove(&key) {
                    return Err(DbError::Cancelled);
                }
                state.waiters.insert(key, cancel_tx);
            }
            let waiter_guard = SqliteWaiterGuard {
                cancel_state: Arc::clone(&self.cancel_state),
                cancel_key: key,
            };
            if flag_cancelled(cancel_flag) {
                return Err(DbError::Cancelled);
            }
            let gate = tokio::select! {
                biased;
                _ = &mut cancel_rx => return Err(DbError::Cancelled),
                guard = Arc::clone(&self.conn_gate).lock_owned() => guard,
            };
            {
                let mut state = self.cancel_state.lock().map_err(|_| DbError::Disconnected)?;
                state.waiters.remove(&key);
                if flag_cancelled(cancel_flag) || state.cancelled_keys.remove(&key) {
                    return Err(DbError::Cancelled);
                }
                state.active_key = Some(key);
                state.cancel_requested = false;
            }
            (gate, Some(waiter_guard))
        } else {
            let gate = Arc::clone(&self.conn_gate).lock_owned().await;
            let mut state = self.cancel_state.lock().map_err(|_| DbError::Disconnected)?;
            state.active_key = None;
            state.cancel_requested = false;
            (gate, None)
        };

        let conn = Arc::clone(&self.conn);
        let cancel_state = Arc::clone(&self.cancel_state);
        let statements = statements.to_vec();
        let outcome = tokio::task::spawn_blocking(move || {
            let _gate = gate;
            let _active_guard = SqliteActiveKeyGuard {
                cancel_state: Arc::clone(&cancel_state),
                cancel_key,
            };
            let mut conn_guard = conn.lock().map_err(|_| DbError::Disconnected)?;
            let is_cancelled = || cancel_state.lock().is_ok_and(|s| s.cancel_requested);
            if is_cancelled() {
                return Err(DbError::Cancelled);
            }

            let batch_start = Instant::now();
            let mut results = Vec::new();
            let mut logs = Vec::new();
            let mut cancelled = false;

            for raw_stmt in &statements {
                if is_cancelled() {
                    cancelled = true;
                    break;
                }
                let stmt_sql = crate::sql::trim_sql_whitespace_for_engine(DbEngine::Sqlite, raw_stmt);
                if stmt_sql.is_empty() {
                    continue;
                }
                let start = Instant::now();
                match execute_single_sqlite_stmt(&mut conn_guard, stmt_sql, row_limit, &is_cancelled) {
                    Ok(stmt_outcome) => {
                        let elapsed_ms = start.elapsed().as_millis() as u64;
                        logs.push(ExecutedSql {
                            sql: stmt_sql.to_owned(),
                            duration_ms: elapsed_ms,
                            error: None,
                        });
                        results.push(QueryStatementResult {
                            sql: stmt_sql.to_owned(),
                            columns: stmt_outcome.columns,
                            rows: stmt_outcome.rows,
                            rows_affected: stmt_outcome.rows_affected,
                            truncated: stmt_outcome.truncated,
                            total_rows_seen: stmt_outcome.total_rows_seen,
                            elapsed_ms,
                            error: None,
                        });
                    }
                    Err(err) => {
                        let elapsed_ms = start.elapsed().as_millis() as u64;
                        if matches!(err, DbError::Cancelled) {
                            cancelled = true;
                        }
                        let err_msg = err.to_string();
                        logs.push(ExecutedSql {
                            sql: stmt_sql.to_owned(),
                            duration_ms: elapsed_ms,
                            error: Some(err_msg.clone()),
                        });
                        results.push(QueryStatementResult {
                            sql: stmt_sql.to_owned(),
                            columns: Vec::new(),
                            rows: Vec::new(),
                            rows_affected: None,
                            truncated: false,
                            total_rows_seen: 0,
                            elapsed_ms,
                            error: Some(err_msg),
                        });
                        break;
                    }
                }
            }

            Ok((
                QueryBatchResult {
                    results,
                    total_elapsed_ms: batch_start.elapsed().as_millis() as u64,
                    cancelled,
                },
                logs,
            ))
        })
        .await
        .map_err(|e| DbError::ConnectionFailed(format!("SQLite worker task failed: {e}")))?;
        drop(_waiter_guard);
        outcome
    }
}

#[async_trait]
impl DbSession for SqliteSession {
    fn engine(&self) -> DbEngine {
        DbEngine::Sqlite
    }

    fn server_info(&self) -> &ServerInfo {
        &self.server_info
    }

    async fn list_databases(&self) -> Result<(Vec<DatabaseInfo>, ExecutedSql), DbError> {
        let name = self.server_info.current_database.clone();
        let file_path = self.file_path.clone();
        let start = Instant::now();
        self.with_conn(|conn| {
            conn.query_row("SELECT 1", [], |row| row.get::<_, i64>(0))
                .map_err(map_sqlite_err)?;
            Ok(())
        })
        .await?;
        let size_bytes = file_path
            .as_deref()
            .and_then(|p| std::fs::metadata(p).ok())
            .and_then(|m| i64::try_from(m.len()).ok());
        Ok((
            vec![DatabaseInfo {
                name,
                owner: String::new(),
                size_bytes,
            }],
            ExecutedSql {
                sql: "PRAGMA database_list".to_owned(),
                duration_ms: start.elapsed().as_millis() as u64,
                error: None,
            },
        ))
    }

    async fn list_schemas(&self) -> Result<(Vec<SchemaInfo>, ExecutedSql), DbError> {
        self.with_conn(|conn| {
            let sql = "PRAGMA database_list";
            let start = Instant::now();
            let mut stmt = conn.prepare(sql).map_err(map_sqlite_err)?;
            let mut rows = stmt.query([]).map_err(map_sqlite_err)?;
            let mut schemas = Vec::new();
            while let Some(row) = rows.next().map_err(map_sqlite_err)? {
                let name: String = row.get(1).map_err(map_sqlite_err)?;
                let is_system = name == "temp";
                schemas.push(SchemaInfo { name, is_system });
            }
            schemas.sort_by(|a, b| {
                let rank = |s: &SchemaInfo| match s.name.as_str() {
                    "main" => 0u8,
                    _ if !s.is_system => 1u8,
                    _ => 2u8,
                };
                rank(a).cmp(&rank(b)).then_with(|| a.name.cmp(&b.name))
            });
            Ok((
                schemas,
                ExecutedSql {
                    sql: sql.to_owned(),
                    duration_ms: start.elapsed().as_millis() as u64,
                    error: None,
                },
            ))
        })
        .await
    }

    async fn list_objects(&self, schema: &str) -> Result<(Vec<DbObject>, ExecutedSql), DbError> {
        let schema_owned = normalize_schema(schema).to_owned();
        let reported_schema = if schema.trim().is_empty() {
            "main".to_owned()
        } else {
            schema.to_owned()
        };
        self.with_conn(move |conn| {
            let master = master_table_ident(&schema_owned);
            let sql = format!(
                "SELECT name, type FROM {master} \
                 WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite_%' \
                 ORDER BY name ASC"
            );
            let start = Instant::now();
            let mut stmt = conn.prepare(&sql).map_err(map_sqlite_err)?;
            let mut rows = stmt.query([]).map_err(map_sqlite_err)?;
            let mut objects = Vec::new();
            while let Some(row) = rows.next().map_err(map_sqlite_err)? {
                let name: String = row.get(0).map_err(map_sqlite_err)?;
                let obj_type: String = row.get(1).map_err(map_sqlite_err)?;
                let kind = match obj_type.as_str() {
                    "table" => DbObjectKind::Table,
                    "view" => DbObjectKind::View,
                    _ => continue,
                };
                objects.push(DbObject {
                    schema: reported_schema.clone(),
                    name,
                    kind,
                    estimated_rows: None,
                    total_bytes: None,
                });
            }
            Ok((
                objects,
                ExecutedSql {
                    sql,
                    duration_ms: start.elapsed().as_millis() as u64,
                    error: None,
                },
            ))
        })
        .await
    }

    async fn list_columns(&self, schema: &str, object: &str) -> Result<(Vec<ColumnSummary>, ExecutedSql), DbError> {
        let schema_owned = normalize_schema(schema).to_owned();
        let object_owned = object.to_owned();
        self.with_conn(move |conn| {
            let sql = format!(
                "PRAGMA {}.table_info({})",
                quote_ident(&schema_owned),
                quote_ident(&object_owned)
            );
            let start = Instant::now();
            let mut stmt = conn.prepare(&sql).map_err(map_sqlite_err)?;
            let mut rows = stmt.query([]).map_err(map_sqlite_err)?;
            let mut cols = Vec::new();
            while let Some(row) = rows.next().map_err(map_sqlite_err)? {
                let name: String = row.get(1).map_err(map_sqlite_err)?;
                let data_type: String = row.get(2).map_err(map_sqlite_err)?;
                let notnull: i64 = row.get(3).map_err(map_sqlite_err)?;
                let pk: i64 = row.get(5).map_err(map_sqlite_err)?;
                cols.push(ColumnSummary {
                    name,
                    data_type,
                    nullable: notnull == 0,
                    is_primary_key: pk > 0,
                });
            }
            Ok((
                cols,
                ExecutedSql {
                    sql,
                    duration_ms: start.elapsed().as_millis() as u64,
                    error: None,
                },
            ))
        })
        .await
    }

    async fn fetch_table_page(&self, req: &PageRequest) -> Result<(TablePage, Vec<ExecutedSql>), DbError> {
        if let Some(where_clause) = req.filter_where.as_deref() {
            crate::safety::validate_table_filter_where(
                DbEngine::Sqlite,
                where_clause,
                crate::sql::SqlSessionMode::default(),
            )
            .map_err(DbError::InvalidInput)?;
        }
        let req = req.clone();
        self.with_conn(move |conn| {
            let schema = normalize_schema(&req.schema);
            let limit = req.limit.clamp(1, 5000);
            let offset = req.offset.max(0);
            let (columns, _) = fetch_column_metas_sync(conn, schema, &req.table)?;

            if columns.is_empty() {
                return Ok((
                    TablePage {
                        columns,
                        rows: Vec::new(),
                        raw_rows: Vec::new(),
                        transaction_support: TableTransactionSupport::Supported,
                        offset,
                        limit,
                        total_rows_estimate: Some(0),
                        total_is_exact: true,
                        has_more: false,
                        elapsed_ms: 0,
                    },
                    Vec::new(),
                ));
            }

            if let Some(sort) = &req.sort
                && !columns.iter().any(|c| c.name == sort.column)
            {
                return Err(DbError::InvalidInput(format!(
                    "unknown sort column: {}",
                    sort.column
                )));
            }

            let sql = build_table_page_sql(schema, &req.table, &columns, &req, limit, offset);
            let start = Instant::now();
            let mut stmt = conn.prepare(&sql).map_err(map_sqlite_err)?;
            let mut raw_rows = stmt.query([]).map_err(map_sqlite_err)?;
            let mut decoded_rows = Vec::new();
            let mut raw_rows_out = Vec::new();
            while let Some(row) = raw_rows.next().map_err(map_sqlite_err)? {
                let mut cells = Vec::with_capacity(columns.len());
                let mut raw_cells = Vec::with_capacity(columns.len());
                for (idx, col) in columns.iter().enumerate() {
                    let val_ref = row.get_ref(idx).map_err(map_sqlite_err)?;
                    raw_cells.push(RawCellValue::Sqlite(sqlite_value_ref_to_raw(val_ref)));
                    cells.push(decode_sqlite_cell(val_ref, &col.data_type));
                }
                decoded_rows.push(cells);
                raw_rows_out.push(raw_cells);
            }
            let elapsed_ms = start.elapsed().as_millis() as u64;

            let mut logs = vec![ExecutedSql {
                sql,
                duration_ms: elapsed_ms,
                error: None,
            }];

            let has_more = decoded_rows.len() > limit as usize;
            decoded_rows.truncate(limit as usize);
            raw_rows_out.truncate(limit as usize);

            let filter_active = req
                .filter_where
                .as_deref()
                .is_some_and(|w| !w.trim().is_empty());

            let (total_rows_estimate, total_is_exact) = if !has_more {
                (Some(offset + decoded_rows.len() as i64), true)
            } else if !filter_active {
                let count_sql = format!(
                    "SELECT COUNT(*) FROM {}.{}",
                    quote_ident(schema),
                    quote_ident(&req.table)
                );
                let count_start = Instant::now();
                let total: i64 = conn
                    .query_row(&count_sql, [], |r| r.get(0))
                    .map_err(map_sqlite_err)?;
                logs.push(ExecutedSql {
                    sql: count_sql,
                    duration_ms: count_start.elapsed().as_millis() as u64,
                    error: None,
                });
                (Some(total), true)
            } else {
                (None, false)
            };

            Ok((
                TablePage {
                    columns,
                    rows: decoded_rows,
                    raw_rows: raw_rows_out,
                    transaction_support: TableTransactionSupport::Supported,
                    offset,
                    limit,
                    total_rows_estimate,
                    total_is_exact,
                    has_more,
                    elapsed_ms,
                },
                logs,
            ))
        })
        .await
    }

    async fn describe_table(&self, schema: &str, table: &str) -> Result<(TableStructure, Vec<ExecutedSql>), DbError> {
        let schema_owned = normalize_schema(schema).to_owned();
        let table_owned = table.to_owned();
        self.with_conn(move |conn| {
            let mut logs = Vec::new();
            let (columns, col_log) = fetch_column_metas_sync(conn, &schema_owned, &table_owned)?;
            logs.push(col_log);

            let (indexes, idx_log) = fetch_indexes_sync(conn, &schema_owned, &table_owned)?;
            logs.push(idx_log);

            let master = master_table_ident(&schema_owned);
            let master_sql = format!("SELECT type, sql FROM {master} WHERE name = ?1");
            let stats_start = Instant::now();
            let obj_entry: Option<(String, Option<String>)> = conn
                .query_row(&master_sql, [&table_owned], |row| Ok((row.get(0)?, row.get(1)?)))
                .ok();

            let mut ddl_parts = Vec::new();
            let mut is_table = false;
            if let Some((obj_type, maybe_sql)) = obj_entry {
                is_table = obj_type == "table";
                if let Some(create_sql) = maybe_sql {
                    let trimmed = create_sql.trim();
                    if !trimmed.is_empty() {
                        ddl_parts.push(ensure_trailing_semicolon(trimmed));
                    }
                }
            }
            for idx in &indexes {
                let def = idx.definition.trim();
                if !def.is_empty() && !def.starts_with("--") {
                    ddl_parts.push(ensure_trailing_semicolon(def));
                }
            }
            logs.push(ExecutedSql {
                sql: format!("{master_sql} -- ?1 = '{}'", table_owned.replace('\'', "''")),
                duration_ms: stats_start.elapsed().as_millis() as u64,
                error: None,
            });

            let estimated_rows = if is_table {
                let count_sql = format!(
                    "SELECT COUNT(*) FROM {}.{}",
                    quote_ident(&schema_owned),
                    quote_ident(&table_owned)
                );
                let count_start = Instant::now();
                let count = conn.query_row(&count_sql, [], |r| r.get::<_, i64>(0)).ok();
                logs.push(ExecutedSql {
                    sql: count_sql,
                    duration_ms: count_start.elapsed().as_millis() as u64,
                    error: None,
                });
                count
            } else {
                None
            };

            let stats = TableStats {
                total_bytes: None,
                data_bytes: None,
                index_bytes: None,
                estimated_rows,
                comment: None,
                ddl: ddl_parts.join("\n\n"),
                transaction_support: TableTransactionSupport::Supported,
            };

            Ok((
                TableStructure {
                    columns,
                    indexes,
                    stats,
                },
                logs,
            ))
        })
        .await
    }

    async fn execute_sql(
        &self,
        statements: &[String],
        row_limit: Option<usize>,
    ) -> Result<(QueryBatchResult, Vec<ExecutedSql>), DbError> {
        self.execute_sql_inner(statements, row_limit, None, None).await
    }

    async fn execute_sql_cancellable(
        &self,
        statements: &[String],
        row_limit: Option<usize>,
        cancel_key: u64,
        cancel_flag: Option<&AtomicBool>,
    ) -> Result<(QueryBatchResult, Vec<ExecutedSql>), DbError> {
        self.execute_sql_inner(statements, row_limit, Some(cancel_key), cancel_flag)
            .await
    }

    async fn execute_transaction(
        &self,
        statements: &[String],
    ) -> Result<(TransactionOutcome, Vec<ExecutedSql>), DbError> {
        if self.read_only {
            return Err(DbError::InvalidInput(
                "Read-Only connection blocks write transactions".to_owned(),
            ));
        }
        if statements.is_empty() {
            return Err(DbError::InvalidInput(
                "no SQL statements to execute in transaction".to_owned(),
            ));
        }
        let statements = statements.to_vec();
        self.with_conn(move |conn| {
            let total_start = Instant::now();
            let mut logs = Vec::with_capacity(statements.len() + 2);
            let mut total_rows_affected = 0u64;
            let mut executed = 0usize;

            let begin_start = Instant::now();
            if let Err(err) = conn.execute_batch("BEGIN") {
                let mapped = map_sqlite_err(err);
                logs.push(ExecutedSql {
                    sql: "BEGIN;".to_owned(),
                    duration_ms: begin_start.elapsed().as_millis() as u64,
                    error: Some(mapped.to_string()),
                });
                return Err(mapped);
            }
            logs.push(ExecutedSql {
                sql: "BEGIN;".to_owned(),
                duration_ms: begin_start.elapsed().as_millis() as u64,
                error: None,
            });

            for raw_stmt in &statements {
                let stmt_sql = crate::sql::trim_sql_whitespace_for_engine(DbEngine::Sqlite, raw_stmt);
                if stmt_sql.is_empty() {
                    continue;
                }
                let stmt_start = Instant::now();
                let res = (|| -> Result<u64, DbError> {
                    let mut prepared = conn.prepare(stmt_sql).map_err(map_sqlite_err)?;
                    if prepared.column_count() > 0 {
                        let mut rows = prepared.query([]).map_err(map_sqlite_err)?;
                        while rows.next().map_err(map_sqlite_err)?.is_some() {}
                    } else {
                        prepared.execute([]).map_err(map_sqlite_err)?;
                    }
                    Ok(conn.changes())
                })();

                match res {
                    Ok(changes) => {
                        total_rows_affected = total_rows_affected.saturating_add(changes);
                        executed += 1;
                        logs.push(ExecutedSql {
                            sql: stmt_sql.to_owned(),
                            duration_ms: stmt_start.elapsed().as_millis() as u64,
                            error: None,
                        });
                    }
                    Err(mapped) => {
                        drop(conn.execute_batch("ROLLBACK"));
                        return Err(mapped);
                    }
                }
            }

            let commit_start = Instant::now();
            match conn.execute_batch("COMMIT") {
                Ok(()) => {
                    logs.push(ExecutedSql {
                        sql: "COMMIT;".to_owned(),
                        duration_ms: commit_start.elapsed().as_millis() as u64,
                        error: None,
                    });
                    Ok((
                        TransactionOutcome {
                            statements_executed: executed,
                            total_rows_affected,
                            elapsed_ms: total_start.elapsed().as_millis() as u64,
                        },
                        logs,
                    ))
                }
                Err(err) => {
                    let mapped = map_sqlite_err(err);
                    drop(conn.execute_batch("ROLLBACK"));
                    Err(mapped)
                }
            }
        })
        .await
    }

    async fn execute_staged_mutations(
        &self,
        batch: &StagedCommitBatch,
    ) -> Result<(TransactionOutcome, Vec<ExecutedSql>), DbError> {
        if self.read_only {
            return Err(DbError::InvalidInput(
                "Read-Only connection blocks write transactions".to_owned(),
            ));
        }
        if batch.mutations.is_empty() {
            return Err(DbError::InvalidInput("no staged changes to execute".to_owned()));
        }
        let batch = batch.clone();
        self.with_conn(move |conn| {
            let schema = normalize_schema(&batch.schema);
            let total_start = Instant::now();
            let mut logs = Vec::with_capacity(batch.mutations.len() + 2);
            let mut total_rows_affected = 0u64;
            let mut executed = 0usize;

            let begin_start = Instant::now();
            if let Err(err) = conn.execute_batch("BEGIN IMMEDIATE") {
                let mapped = map_sqlite_err(err);
                logs.push(ExecutedSql {
                    sql: "BEGIN IMMEDIATE;".to_owned(),
                    duration_ms: begin_start.elapsed().as_millis() as u64,
                    error: Some(mapped.to_string()),
                });
                return Err(mapped);
            }
            logs.push(ExecutedSql {
                sql: "BEGIN IMMEDIATE;".to_owned(),
                duration_ms: begin_start.elapsed().as_millis() as u64,
                error: None,
            });

            for mutation in &batch.mutations {
                let (sql, params, op_kind, row_label) =
                    match build_sqlite_staged_mutation_sql(schema, &batch.table, mutation) {
                        Ok(plan) => plan,
                        Err(err) => {
                            drop(conn.execute_batch("ROLLBACK"));
                            return Err(err);
                        }
                    };
                let stmt_start = Instant::now();
                let res = (|| -> Result<u64, DbError> {
                    let mut prepared = conn.prepare(&sql).map_err(map_sqlite_err)?;
                    prepared
                        .execute(params_from_iter(params.iter()))
                        .map_err(map_sqlite_err)?;
                    Ok(conn.changes())
                })();
                match res {
                    Ok(1) => {
                        total_rows_affected = total_rows_affected.saturating_add(1);
                        executed += 1;
                        logs.push(ExecutedSql {
                            sql,
                            duration_ms: stmt_start.elapsed().as_millis() as u64,
                            error: None,
                        });
                    }
                    Ok(changes) => {
                        drop(conn.execute_batch("ROLLBACK"));
                        let msg = if changes == 0 {
                            format!(
                                "Staged {op_kind} ({row_label}) matched 0 rows: the row was modified or deleted since it was loaded. Transaction rolled back; review your staged changes and reload."
                            )
                        } else {
                            format!(
                                "Staged {op_kind} ({row_label}) matched {changes} rows instead of 1. Transaction rolled back to prevent unintended changes."
                            )
                        };
                        return Err(DbError::StaleRowConflict(msg));
                    }
                    Err(mapped) => {
                        drop(conn.execute_batch("ROLLBACK"));
                        return Err(mapped);
                    }
                }
            }

            let commit_start = Instant::now();
            match conn.execute_batch("COMMIT") {
                Ok(()) => {
                    logs.push(ExecutedSql {
                        sql: "COMMIT;".to_owned(),
                        duration_ms: commit_start.elapsed().as_millis() as u64,
                        error: None,
                    });
                    Ok((
                        TransactionOutcome {
                            statements_executed: executed,
                            total_rows_affected,
                            elapsed_ms: total_start.elapsed().as_millis() as u64,
                        },
                        logs,
                    ))
                }
                Err(err) => {
                    let mapped = map_sqlite_err(err);
                    drop(conn.execute_batch("ROLLBACK"));
                    Err(mapped)
                }
            }
        })
        .await
    }

    async fn explain_sql(&self, statement: &str, _analyze: bool) -> Result<(ExplainPlan, ExecutedSql), DbError> {
        let trimmed = crate::sql::trim_sql_whitespace_for_engine(DbEngine::Sqlite, statement).to_owned();
        if trimmed.is_empty() {
            return Err(DbError::InvalidInput(
                "cannot explain an empty SQL statement".to_owned(),
            ));
        }
        let explain_query = build_explain_sql_for_engine(DbEngine::Sqlite, &trimmed, false);
        self.with_conn(move |conn| {
            let start = Instant::now();
            let mut stmt = conn.prepare(&explain_query).map_err(map_sqlite_err)?;
            let mut rows = stmt.query([]).map_err(map_sqlite_err)?;
            let mut plan_rows = Vec::new();
            while let Some(row) = rows.next().map_err(map_sqlite_err)? {
                let id: i64 = row.get(0).map_err(map_sqlite_err)?;
                let parent: i64 = row.get(1).map_err(map_sqlite_err)?;
                let detail: String = row.get(3).map_err(map_sqlite_err)?;
                plan_rows.push((id, parent, detail));
            }
            let duration_ms = start.elapsed().as_millis() as u64;
            let plan = ExplainPlan::parse_sqlite_query_plan(trimmed, &plan_rows)
                .map_err(|e| DbError::InvalidInput(e.to_string()))?;
            Ok((
                plan,
                ExecutedSql {
                    sql: explain_query,
                    duration_ms,
                    error: None,
                },
            ))
        })
        .await
    }

    async fn fetch_fk_row(
        &self,
        fk: &ForeignKeyRef,
        value: &CellValue,
        source_col: &ColumnMeta,
    ) -> Result<(FkPeekRow, ExecutedSql), DbError> {
        let Some(where_clause) =
            build_fk_where_clause_for_engine(DbEngine::Sqlite, &fk.target_column, value, source_col)
        else {
            return Err(DbError::InvalidInput("cannot peek NULL foreign-key value".to_owned()));
        };

        let fk = fk.clone();
        let lookup_display = format!("{} = {}", fk.target_column, value.display_compact());
        self.with_conn(move |conn| {
            let target_schema = normalize_schema(&fk.target_schema);
            let (target_columns, _) = fetch_column_metas_sync(conn, target_schema, &fk.target_table)?;
            if target_columns.is_empty() {
                return Err(DbError::InvalidInput(format!(
                    "target table {target_schema}.{} has no columns",
                    fk.target_table
                )));
            }

            let req = PageRequest {
                schema: target_schema.to_owned(),
                table: fk.target_table.clone(),
                filter_where: Some(where_clause),
                sort: None,
                limit: 1,
                offset: 0,
            };
            let sql = build_table_page_sql(target_schema, &fk.target_table, &target_columns, &req, 1, 0);
            let start = Instant::now();
            let mut stmt = conn.prepare(&sql).map_err(map_sqlite_err)?;
            let mut raw_rows = stmt.query([]).map_err(map_sqlite_err)?;
            let row = if let Some(first) = raw_rows.next().map_err(map_sqlite_err)? {
                let mut cells = Vec::with_capacity(target_columns.len());
                for (idx, col) in target_columns.iter().enumerate() {
                    let val_ref = first.get_ref(idx).map_err(map_sqlite_err)?;
                    cells.push(decode_sqlite_cell(val_ref, &col.data_type));
                }
                Some(cells)
            } else {
                None
            };
            let elapsed_ms = start.elapsed().as_millis() as u64;

            Ok((
                FkPeekRow {
                    fk,
                    lookup_display,
                    columns: target_columns,
                    row,
                    elapsed_ms,
                },
                ExecutedSql {
                    sql,
                    duration_ms: elapsed_ms,
                    error: None,
                },
            ))
        })
        .await
    }

    async fn cancel_running_query(&self) -> Result<(), DbError> {
        let mut state = self.cancel_state.lock().map_err(|_| DbError::Disconnected)?;
        state.cancel_requested = true;
        self.interrupt_handle.interrupt();
        Ok(())
    }

    async fn cancel_query_by_key(&self, cancel_key: u64) -> Result<(), DbError> {
        let should_retry = {
            let mut state = self.cancel_state.lock().map_err(|_| DbError::Disconnected)?;
            if state.active_key == Some(cancel_key) {
                state.cancel_requested = true;
                self.interrupt_handle.interrupt();
                true
            } else {
                if let Some(tx) = state.waiters.remove(&cancel_key) {
                    state.cancelled_keys.insert(cancel_key);
                    tx.send(()).unwrap_or(());
                }
                false
            }
        };
        if should_retry {
            for _ in 0..5 {
                tokio::time::sleep(Duration::from_millis(2)).await;
                let Ok(state) = self.cancel_state.lock() else {
                    break;
                };
                if state.active_key != Some(cancel_key) || !state.cancel_requested {
                    break;
                }
                self.interrupt_handle.interrupt();
            }
        }
        Ok(())
    }

    async fn ping(&self) -> Result<(), DbError> {
        self.with_conn(|conn| {
            conn.query_row("SELECT 1", [], |row| row.get::<_, i64>(0))
                .map_err(map_sqlite_err)?;
            Ok(())
        })
        .await
    }
}

struct SingleStmtOutcome {
    columns: Vec<ColumnMeta>,
    rows: Vec<Vec<CellValue>>,
    rows_affected: Option<u64>,
    truncated: bool,
    total_rows_seen: usize,
}

fn execute_single_sqlite_stmt<F>(
    conn: &mut Connection,
    sql: &str,
    row_limit: Option<usize>,
    is_cancelled: &F,
) -> Result<SingleStmtOutcome, DbError>
where
    F: Fn() -> bool,
{
    if is_cancelled() {
        return Err(DbError::Cancelled);
    }
    let mut prepared = conn.prepare(sql).map_err(map_sqlite_err)?;
    if is_cancelled() {
        return Err(DbError::Cancelled);
    }
    let col_count = prepared.column_count();
    if col_count == 0 {
        let affected = prepared.execute([]).map_err(map_sqlite_err)?;
        return Ok(SingleStmtOutcome {
            columns: Vec::new(),
            rows: Vec::new(),
            rows_affected: Some(affected as u64),
            truncated: false,
            total_rows_seen: 0,
        });
    }

    let mut columns: Vec<ColumnMeta> = prepared
        .column_names()
        .map(|name| ColumnMeta {
            name: name.to_owned(),
            data_type: String::new(),
            udt_name: String::new(),
            nullable: true,
            default_expr: None,
            is_primary_key: false,
            is_unique: false,
            comment: None,
            foreign_key: None,
        })
        .collect();

    let mut rows = Vec::new();
    let mut truncated = false;
    let mut total_rows_seen = 0usize;
    let mut query_rows = prepared.query([]).map_err(map_sqlite_err)?;

    while let Some(row) = query_rows.next().map_err(map_sqlite_err)? {
        if is_cancelled() {
            return Err(DbError::Cancelled);
        }
        total_rows_seen += 1;
        if row_limit.is_none_or(|lim| rows.len() < lim) {
            let mut decoded = Vec::with_capacity(col_count);
            for (idx, col) in columns.iter_mut().enumerate() {
                let val_ref = row.get_ref(idx).map_err(map_sqlite_err)?;
                let cell = decode_sqlite_cell(val_ref, &col.data_type);
                if col.data_type.is_empty()
                    && let Some(inferred) = infer_type_from_cell(&cell)
                {
                    col.data_type = inferred.to_owned();
                    col.udt_name = inferred.to_owned();
                }
                decoded.push(cell);
            }
            rows.push(decoded);
        } else {
            truncated = true;
        }
    }

    for col in &mut columns {
        if col.data_type.is_empty() {
            col.data_type = "text".to_owned();
            col.udt_name = "text".to_owned();
        }
    }

    Ok(SingleStmtOutcome {
        columns,
        rows,
        rows_affected: None,
        truncated,
        total_rows_seen,
    })
}

fn fetch_column_metas_sync(
    conn: &Connection,
    schema: &str,
    table: &str,
) -> Result<(Vec<ColumnMeta>, ExecutedSql), DbError> {
    let info_sql = format!("PRAGMA {}.table_info({})", quote_ident(schema), quote_ident(table));
    let start = Instant::now();
    let mut info_stmt = conn.prepare(&info_sql).map_err(map_sqlite_err)?;
    let mut info_rows = info_stmt.query([]).map_err(map_sqlite_err)?;

    let mut raw_cols = Vec::new();
    while let Some(row) = info_rows.next().map_err(map_sqlite_err)? {
        let name: String = row.get(1).map_err(map_sqlite_err)?;
        let data_type: String = row.get(2).map_err(map_sqlite_err)?;
        let notnull: i64 = row.get(3).map_err(map_sqlite_err)?;
        let default_expr: Option<String> = row.get(4).map_err(map_sqlite_err)?;
        let pk: i64 = row.get(5).map_err(map_sqlite_err)?;
        raw_cols.push((name, data_type, notnull == 0, default_expr, pk > 0));
    }

    if raw_cols.is_empty() {
        return Ok((
            Vec::new(),
            ExecutedSql {
                sql: info_sql,
                duration_ms: start.elapsed().as_millis() as u64,
                error: None,
            },
        ));
    }

    // Foreign keys
    let fk_sql = format!(
        "PRAGMA {}.foreign_key_list({})",
        quote_ident(schema),
        quote_ident(table)
    );
    let mut fk_stmt = conn.prepare(&fk_sql).map_err(map_sqlite_err)?;
    let mut fk_rows = fk_stmt.query([]).map_err(map_sqlite_err)?;
    let mut fks: HashMap<String, ForeignKeyRef> = HashMap::new();
    while let Some(row) = fk_rows.next().map_err(map_sqlite_err)? {
        let id: i64 = row.get(0).map_err(map_sqlite_err)?;
        let target_table: String = row.get(2).map_err(map_sqlite_err)?;
        let from_col: String = row.get(3).map_err(map_sqlite_err)?;
        let to_col_opt: Option<String> = row.get(4).map_err(map_sqlite_err)?;
        let target_column = match to_col_opt {
            Some(c) if !c.is_empty() => c,
            _ => lookup_first_pk_column(conn, schema, &target_table)?.unwrap_or_default(),
        };
        fks.entry(from_col.clone()).or_insert_with(|| ForeignKeyRef {
            constraint_name: format!("fk_{table}_{from_col}_{id}"),
            target_schema: schema.to_owned(),
            target_table,
            target_column,
        });
    }

    // Single-column unique indexes (non-PK, non-partial)
    let unique_cols = fetch_single_col_uniques_sync(conn, schema, table)?;

    let columns = raw_cols
        .into_iter()
        .map(|(name, data_type, nullable, default_expr, is_primary_key)| {
            let udt_name = data_type.trim().to_ascii_lowercase();
            let is_unique = is_primary_key || unique_cols.contains(&name);
            let foreign_key = fks.remove(&name);
            ColumnMeta {
                name,
                data_type,
                udt_name,
                nullable,
                default_expr,
                is_primary_key,
                is_unique,
                comment: None,
                foreign_key,
            }
        })
        .collect();

    Ok((
        columns,
        ExecutedSql {
            sql: info_sql,
            duration_ms: start.elapsed().as_millis() as u64,
            error: None,
        },
    ))
}

fn lookup_first_pk_column(conn: &Connection, schema: &str, table: &str) -> Result<Option<String>, DbError> {
    let sql = format!("PRAGMA {}.table_info({})", quote_ident(schema), quote_ident(table));
    let mut stmt = conn.prepare(&sql).map_err(map_sqlite_err)?;
    let mut rows = stmt.query([]).map_err(map_sqlite_err)?;
    while let Some(row) = rows.next().map_err(map_sqlite_err)? {
        let name: String = row.get(1).map_err(map_sqlite_err)?;
        let pk: i64 = row.get(5).map_err(map_sqlite_err)?;
        if pk > 0 {
            return Ok(Some(name));
        }
    }
    Ok(None)
}

fn fetch_single_col_uniques_sync(
    conn: &Connection,
    schema: &str,
    table: &str,
) -> Result<HashSet<String>, DbError> {
    let list_sql = format!("PRAGMA {}.index_list({})", quote_ident(schema), quote_ident(table));
    let mut list_stmt = conn.prepare(&list_sql).map_err(map_sqlite_err)?;
    let mut list_rows = list_stmt.query([]).map_err(map_sqlite_err)?;
    let mut unique_names = Vec::new();
    while let Some(row) = list_rows.next().map_err(map_sqlite_err)? {
        let name: String = row.get(1).map_err(map_sqlite_err)?;
        let unique: i64 = row.get(2).map_err(map_sqlite_err)?;
        let origin: String = row.get(3).map_err(map_sqlite_err)?;
        let partial: i64 = row.get(4).map_err(map_sqlite_err)?;
        if unique != 0 && origin != "pk" && partial == 0 {
            unique_names.push(name);
        }
    }

    let mut cols = HashSet::new();
    for idx_name in unique_names {
        let idx_cols = fetch_index_columns_sync(conn, schema, &idx_name)?;
        if idx_cols.len() == 1
            && let Some(col_name) = idx_cols.into_iter().next()
            && col_name != "<expr>"
        {
            cols.insert(col_name);
        }
    }
    Ok(cols)
}

fn fetch_indexes_sync(
    conn: &Connection,
    schema: &str,
    table: &str,
) -> Result<(Vec<IndexInfo>, ExecutedSql), DbError> {
    let list_sql = format!("PRAGMA {}.index_list({})", quote_ident(schema), quote_ident(table));
    let start = Instant::now();
    let mut list_stmt = conn.prepare(&list_sql).map_err(map_sqlite_err)?;
    let mut list_rows = list_stmt.query([]).map_err(map_sqlite_err)?;

    let mut entries = Vec::new();
    while let Some(row) = list_rows.next().map_err(map_sqlite_err)? {
        let name: String = row.get(1).map_err(map_sqlite_err)?;
        let unique: i64 = row.get(2).map_err(map_sqlite_err)?;
        let origin: String = row.get(3).map_err(map_sqlite_err)?;
        entries.push((name, unique != 0, origin == "pk"));
    }

    let master = master_table_ident(schema);
    let sql_lookup = format!("SELECT name, sql FROM {master} WHERE type = 'index' AND tbl_name = ?1");
    let mut sql_stmt = conn.prepare(&sql_lookup).map_err(map_sqlite_err)?;
    let mut sql_rows = sql_stmt.query([table]).map_err(map_sqlite_err)?;
    let mut sql_map: HashMap<String, Option<String>> = HashMap::new();
    while let Some(row) = sql_rows.next().map_err(map_sqlite_err)? {
        let name: String = row.get(0).map_err(map_sqlite_err)?;
        let ddl: Option<String> = row.get(1).map_err(map_sqlite_err)?;
        sql_map.insert(name, ddl);
    }

    let mut indexes = Vec::with_capacity(entries.len());
    for (name, is_unique, is_primary) in entries {
        let columns = fetch_index_columns_sync(conn, schema, &name)?;
        let definition = sql_map
            .get(&name)
            .and_then(|opt| opt.clone())
            .unwrap_or_default();
        indexes.push(IndexInfo {
            name,
            columns,
            is_unique,
            is_primary,
            method: "btree".to_owned(),
            size_bytes: None,
            definition,
        });
    }

    indexes.sort_by(|a, b| {
        b.is_primary
            .cmp(&a.is_primary)
            .then_with(|| b.is_unique.cmp(&a.is_unique))
            .then_with(|| a.name.cmp(&b.name))
    });

    Ok((
        indexes,
        ExecutedSql {
            sql: list_sql,
            duration_ms: start.elapsed().as_millis() as u64,
            error: None,
        },
    ))
}

fn fetch_index_columns_sync(conn: &Connection, schema: &str, index_name: &str) -> Result<Vec<String>, DbError> {
    let info_sql = format!(
        "PRAGMA {}.index_info({})",
        quote_ident(schema),
        quote_ident(index_name)
    );
    let mut stmt = conn.prepare(&info_sql).map_err(map_sqlite_err)?;
    let mut rows = stmt.query([]).map_err(map_sqlite_err)?;
    let mut seq_cols: Vec<(i64, String)> = Vec::new();
    while let Some(row) = rows.next().map_err(map_sqlite_err)? {
        let seqno: i64 = row.get(0).map_err(map_sqlite_err)?;
        let col_name: Option<String> = row.get(2).map_err(map_sqlite_err)?;
        seq_cols.push((seqno, col_name.unwrap_or_else(|| "<expr>".to_owned())));
    }
    seq_cols.sort_by_key(|(seq, _)| *seq);
    Ok(seq_cols.into_iter().map(|(_, name)| name).collect())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SqliteTypeHint {
    Bool,
    Json,
    Date,
    Time,
    Timestamp,
    Uuid,
    Numeric,
    None,
}

fn classify_declared_type(decl_type: &str) -> SqliteTypeHint {
    let upper = decl_type.trim().to_ascii_uppercase();
    if upper.starts_with("TINYINT(1)") {
        return SqliteTypeHint::Bool;
    }
    let base = upper.split('(').next().unwrap_or(&upper).trim();
    match base {
        "BOOL" | "BOOLEAN" => SqliteTypeHint::Bool,
        "JSON" | "JSONB" => SqliteTypeHint::Json,
        "DATE" => SqliteTypeHint::Date,
        "TIME" | "TIMETZ" => SqliteTypeHint::Time,
        "DATETIME" | "TIMESTAMP" | "TIMESTAMPTZ" => SqliteTypeHint::Timestamp,
        "UUID" => SqliteTypeHint::Uuid,
        "NUMERIC" | "DECIMAL" => SqliteTypeHint::Numeric,
        _ => SqliteTypeHint::None,
    }
}

fn sqlite_value_ref_to_raw(val: ValueRef<'_>) -> SqliteRawValue {
    match val {
        ValueRef::Null => SqliteRawValue::Null,
        ValueRef::Integer(i) => SqliteRawValue::Integer(i),
        ValueRef::Real(f) => SqliteRawValue::Real(f),
        ValueRef::Text(bytes) => match std::str::from_utf8(bytes) {
            Ok(s) => SqliteRawValue::Text(s.to_owned()),
            Err(_) => SqliteRawValue::NonUtf8Text(bytes.to_vec()),
        },
        ValueRef::Blob(bytes) => SqliteRawValue::Blob(bytes.to_vec()),
    }
}

fn sqlite_assignment_to_bound(col: &ColumnMeta, val: &StagedCellValue) -> Result<SqliteValue, DbError> {
    match val {
        StagedCellValue::Null => Ok(SqliteValue::Null),
        StagedCellValue::DefaultExpr => Err(DbError::InvalidInput(format!(
            "SQLite does not support DEFAULT assignment in UPDATE for column '{}'",
            col.name
        ))),
        StagedCellValue::Literal(raw) => {
            let trimmed = raw.trim();
            Ok(match classify_column_type(col) {
                LogicalColType::Bool => match trimmed.to_ascii_lowercase().as_str() {
                    "true" | "t" | "1" | "yes" | "y" | "on" => SqliteValue::Integer(1),
                    "false" | "f" | "0" | "no" | "n" | "off" => SqliteValue::Integer(0),
                    _ => SqliteValue::Text(trimmed.to_owned()),
                },
                LogicalColType::Int => match trimmed.parse::<i64>() {
                    Ok(i) => SqliteValue::Integer(i),
                    Err(_) => SqliteValue::Text(trimmed.to_owned()),
                },
                LogicalColType::Float => match trimmed.parse::<f64>() {
                    Ok(f) if f.is_finite() => SqliteValue::Real(f),
                    _ => SqliteValue::Text(trimmed.to_owned()),
                },
                LogicalColType::Numeric => {
                    if let Ok(i) = trimmed.parse::<i64>() {
                        SqliteValue::Integer(i)
                    } else if trimmed.parse::<f64>().is_ok_and(f64::is_finite) {
                        SqliteValue::Real(trimmed.parse::<f64>().unwrap_or_default())
                    } else {
                        SqliteValue::Text(trimmed.to_owned())
                    }
                }
                LogicalColType::Bytes => {
                    let hex = strip_hex_prefix(trimmed);
                    match decode_hex(hex) {
                        Some(bytes) => SqliteValue::Blob(bytes),
                        None => SqliteValue::Text(raw.clone()),
                    }
                }
                LogicalColType::Json | LogicalColType::Text | LogicalColType::Array => {
                    SqliteValue::Text(raw.clone())
                }
                LogicalColType::Date
                | LogicalColType::Time
                | LogicalColType::Timestamp
                | LogicalColType::TimestampTz
                | LogicalColType::Uuid => SqliteValue::Text(trimmed.to_owned()),
            })
        }
    }
}

fn sqlite_col_val_to_bound(val: &StagedColumnValue) -> Result<(SqliteValue, Option<&'static str>), DbError> {
    if let Some(RawCellValue::Sqlite(raw)) = &val.raw {
        return match raw {
            SqliteRawValue::Null => Ok((SqliteValue::Null, Some("null"))),
            SqliteRawValue::Integer(i) => Ok((SqliteValue::Integer(*i), Some("integer"))),
            SqliteRawValue::Real(f) => Ok((SqliteValue::Real(*f), Some("real"))),
            SqliteRawValue::Text(s) => Ok((SqliteValue::Text(s.clone()), Some("text"))),
            SqliteRawValue::NonUtf8Text(_) => Err(DbError::InvalidInput(format!(
                "row contains non-UTF-8 SQLite TEXT in column '{}' and cannot be bound",
                val.column.name
            ))),
            SqliteRawValue::Blob(b) => Ok((SqliteValue::Blob(b.clone()), Some("blob"))),
        };
    }
    Ok(match &val.display {
        CellValue::Null => (SqliteValue::Null, Some("null")),
        CellValue::Bool(b) => (SqliteValue::Integer(i64::from(*b)), None),
        CellValue::Int(i) => (SqliteValue::Integer(*i), Some("integer")),
        CellValue::Float(f) if f.is_finite() => (SqliteValue::Real(*f), Some("real")),
        CellValue::Bytes(b) => (SqliteValue::Blob(b.clone()), Some("blob")),
        other => (SqliteValue::Text(other.copy_text()), None),
    })
}

fn push_sqlite_value_predicate(
    clauses: &mut Vec<String>,
    params: &mut Vec<SqliteValue>,
    quoted_col: &str,
    bound: SqliteValue,
    tname: Option<&'static str>,
) {
    if matches!(bound, SqliteValue::Null) {
        clauses.push(format!("{quoted_col} IS NULL"));
        return;
    }
    // Use BINARY collation on text predicates so NOCASE columns still detect case-only drift.
    let lhs = if tname == Some("text") || matches!(bound, SqliteValue::Text(_)) {
        format!("{quoted_col} COLLATE BINARY")
    } else {
        quoted_col.to_owned()
    };
    if let Some(t) = tname {
        clauses.push(format!("{lhs} = ? AND typeof({quoted_col}) = ?"));
        params.push(bound);
        params.push(SqliteValue::Text(t.to_owned()));
    } else {
        clauses.push(format!("{lhs} = ?"));
        params.push(bound);
    }
}

fn append_sqlite_predicates(
    clauses: &mut Vec<String>,
    params: &mut Vec<SqliteValue>,
    pk_predicates: &[StagedColumnValue],
    concurrency_predicates: &[StagedColumnValue],
) -> Result<(), DbError> {
    if pk_predicates.is_empty() {
        return Err(DbError::InvalidInput(
            "table requires a primary key to update or delete existing rows safely".to_owned(),
        ));
    }
    for pk in pk_predicates {
        let q = quote_ident(&pk.column.name);
        let (bound, tname) = sqlite_col_val_to_bound(pk)?;
        if matches!(bound, SqliteValue::Null) {
            return Err(DbError::InvalidInput(format!(
                "primary key column '{}' cannot be NULL",
                pk.column.name
            )));
        }
        push_sqlite_value_predicate(clauses, params, &q, bound, tname);
    }
    for col in concurrency_predicates {
        let q = quote_ident(&col.column.name);
        let (bound, tname) = sqlite_col_val_to_bound(col)?;
        push_sqlite_value_predicate(clauses, params, &q, bound, tname);
    }
    Ok(())
}

fn build_sqlite_staged_mutation_sql(
    schema: &str,
    table: &str,
    mutation: &StagedMutation,
) -> Result<(String, Vec<SqliteValue>, &'static str, String), DbError> {
    let fq_table = format!("{}.{}", quote_ident(schema), quote_ident(table));
    match mutation {
        StagedMutation::Update {
            row_label,
            assignments,
            pk_predicates,
            concurrency_predicates,
        } => {
            if assignments.is_empty() {
                return Err(DbError::InvalidInput("update has no column assignments".to_owned()));
            }
            let mut set_clauses = Vec::with_capacity(assignments.len());
            let mut params = Vec::with_capacity(assignments.len() + pk_predicates.len() * 2 + concurrency_predicates.len() * 2);
            for a in assignments {
                set_clauses.push(format!("{} = ?", quote_ident(&a.column.name)));
                params.push(sqlite_assignment_to_bound(&a.column, &a.value)?);
            }
            let mut where_clauses = Vec::new();
            append_sqlite_predicates(
                &mut where_clauses,
                &mut params,
                pk_predicates,
                concurrency_predicates,
            )?;
            let sql = format!(
                "UPDATE {fq_table} SET {} WHERE {};",
                set_clauses.join(", "),
                where_clauses.join(" AND ")
            );
            Ok((sql, params, "update", row_label.clone()))
        }
        StagedMutation::Insert {
            row_label,
            assignments,
        } => {
            if assignments.is_empty() {
                return Ok((
                    format!("INSERT INTO {fq_table} DEFAULT VALUES;"),
                    Vec::new(),
                    "insert",
                    row_label.clone(),
                ));
            }
            let mut col_names = Vec::with_capacity(assignments.len());
            let mut placeholders = Vec::with_capacity(assignments.len());
            let mut params = Vec::with_capacity(assignments.len());
            for a in assignments {
                col_names.push(quote_ident(&a.column.name));
                placeholders.push("?".to_owned());
                params.push(sqlite_assignment_to_bound(&a.column, &a.value)?);
            }
            let sql = format!(
                "INSERT INTO {fq_table} ({}) VALUES ({});",
                col_names.join(", "),
                placeholders.join(", ")
            );
            Ok((sql, params, "insert", row_label.clone()))
        }
        StagedMutation::Delete {
            row_label,
            pk_predicates,
            concurrency_predicates,
        } => {
            let mut params = Vec::new();
            let mut where_clauses = Vec::new();
            append_sqlite_predicates(
                &mut where_clauses,
                &mut params,
                pk_predicates,
                concurrency_predicates,
            )?;
            let sql = format!(
                "DELETE FROM {fq_table} WHERE {};",
                where_clauses.join(" AND ")
            );
            Ok((sql, params, "delete", row_label.clone()))
        }
    }
}

fn decode_sqlite_cell(val: ValueRef<'_>, decl_type: &str) -> CellValue {
    let hint = classify_declared_type(decl_type);
    match val {
        ValueRef::Null => CellValue::Null,
        ValueRef::Integer(i) => match hint {
            SqliteTypeHint::Bool => match i {
                0 => CellValue::Bool(false),
                1 => CellValue::Bool(true),
                other => CellValue::Int(other),
            },
            SqliteTypeHint::Numeric => CellValue::Numeric(i.to_string()),
            _ => CellValue::Int(i),
        },
        ValueRef::Real(f) => match hint {
            SqliteTypeHint::Numeric if f.is_finite() => CellValue::Numeric(f.to_string()),
            _ => CellValue::Float(f),
        },
        ValueRef::Text(bytes) => {
            let s = String::from_utf8_lossy(bytes).into_owned();
            match hint {
                SqliteTypeHint::Bool => match s.trim().to_ascii_lowercase().as_str() {
                    "true" | "t" | "1" | "yes" | "y" | "on" => CellValue::Bool(true),
                    "false" | "f" | "0" | "no" | "n" | "off" => CellValue::Bool(false),
                    _ => CellValue::Text(s),
                },
                SqliteTypeHint::Json => CellValue::Json(s),
                SqliteTypeHint::Date => CellValue::Date(s),
                SqliteTypeHint::Time => CellValue::Time(s),
                SqliteTypeHint::Timestamp => CellValue::parse_from_pg_text("timestamptz", Some(s)),
                SqliteTypeHint::Uuid => match uuid::Uuid::parse_str(s.trim()) {
                    Ok(u) => CellValue::Uuid(u),
                    Err(_) => CellValue::Text(s),
                },
                SqliteTypeHint::Numeric => CellValue::Numeric(s),
                SqliteTypeHint::None => CellValue::Text(s),
            }
        }
        ValueRef::Blob(bytes) => CellValue::Bytes(bytes.to_vec()),
    }
}

fn infer_type_from_cell(cell: &CellValue) -> Option<&'static str> {
    match cell {
        CellValue::Null => None,
        CellValue::Bool(_) => Some("boolean"),
        CellValue::Int(_) => Some("integer"),
        CellValue::Float(_) => Some("real"),
        CellValue::Numeric(_) => Some("numeric"),
        CellValue::Uuid(_) => Some("uuid"),
        CellValue::Timestamp { .. } => Some("timestamp"),
        CellValue::Date(_) => Some("date"),
        CellValue::Time(_) => Some("time"),
        CellValue::Json(_) => Some("json"),
        CellValue::Bytes(_) => Some("blob"),
        CellValue::Array(_) | CellValue::Text(_) => Some("text"),
    }
}

fn build_table_page_sql(
    schema: &str,
    table: &str,
    columns: &[ColumnMeta],
    req: &PageRequest,
    limit: i64,
    offset: i64,
) -> String {
    let projections = columns
        .iter()
        .map(|c| quote_ident(&c.name))
        .collect::<Vec<_>>()
        .join(", ");

    let mut sql = format!(
        "SELECT {projections} FROM {}.{}",
        quote_ident(schema),
        quote_ident(table)
    );
    if let Some(where_clause) = &req.filter_where {
        let trimmed = where_clause.trim();
        if !trimmed.is_empty() {
            sql.push_str(&format!(" WHERE ({trimmed})"));
        }
    }
    if let Some(sort) = &req.sort {
        let dir = if sort.ascending { "ASC" } else { "DESC" };
        sql.push_str(&format!(" ORDER BY {} {dir} NULLS LAST", quote_ident(&sort.column)));
    }
    sql.push_str(&format!(" LIMIT {} OFFSET {}", limit + 1, offset));
    sql
}

fn quote_ident(ident: &str) -> String {
    DbEngine::Sqlite.quote_ident(ident)
}

fn normalize_schema(schema: &str) -> &str {
    let trimmed = schema.trim();
    if trimmed.is_empty() { "main" } else { trimmed }
}

fn master_table_ident(schema: &str) -> String {
    if schema.eq_ignore_ascii_case("temp") {
        "\"temp\".\"sqlite_temp_master\"".to_owned()
    } else {
        format!("{}.\"sqlite_master\"", quote_ident(schema))
    }
}

fn ensure_trailing_semicolon(sql: &str) -> String {
    let trimmed = sql.trim_end();
    if trimmed.ends_with(';') {
        trimmed.to_owned()
    } else {
        format!("{trimmed};")
    }
}

fn is_in_memory_path(path: &Path) -> bool {
    path == Path::new(":memory:") || path.to_string_lossy().trim() == ":memory:"
}

fn map_sqlite_err(err: rusqlite::Error) -> DbError {
    match &err {
        rusqlite::Error::SqliteFailure(ffi_err, _) => {
            if ffi_err.code == ErrorCode::OperationInterrupted {
                return DbError::Cancelled;
            }
            DbError::QueryFailed {
                message: err.to_string(),
                code: Some(format!("{:?}", ffi_err.code)),
                detail: None,
                hint: None,
            }
        }
        rusqlite::Error::SqlInputError { msg, sql, offset, .. } => DbError::QueryFailed {
            message: msg.clone(),
            code: None,
            detail: Some(format!("at offset {offset} in: {sql}")),
            hint: None,
        },
        _ => DbError::QueryFailed {
            message: err.to_string(),
            code: None,
            detail: None,
            hint: None,
        },
    }
}
