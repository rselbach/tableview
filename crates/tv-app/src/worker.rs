use std::collections::HashMap;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use rune::Waker;
use rune::credentials::Credentials;
use tv_core::engine::postgres::{PostgresSession, quote_ident};
use tv_core::{
    CellValue, ColumnMeta, ColumnSummary, ConnectionId, ConnectionProfile, DatabaseInfo, DbError, DbObject,
    DbSession, ExecutedSql, ExplainPlan, FkPeekRow, ForeignKeyRef, NewHistoryEntry, PageRequest, PasswordMode,
    QueryBatchResult, QueryHistoryEntry, QueryStore, RequestId, SavedQuery, SavedQueryId, SchemaInfo, ServerInfo,
    SessionId, TabId, TablePage, TableStructure, TransactionOutcome,
};

const QUERY_HISTORY_LIMIT: usize = 200;

/// Commands dispatched from the UI thread to the background database worker.
#[derive(Debug)]
pub enum WorkerCmd {
    Connect {
        session_id: SessionId,
        req_id: RequestId,
        profile: ConnectionProfile,
        prompt_password: Option<String>,
        database: Option<String>,
    },
    SelectDatabase {
        session_id: SessionId,
        req_id: RequestId,
        profile: ConnectionProfile,
        prompt_password: Option<String>,
        database: String,
    },
    SelectSchema {
        session_id: SessionId,
        req_id: RequestId,
        schema: String,
    },
    LoadColumns {
        session_id: SessionId,
        req_id: RequestId,
        schema: String,
        object: String,
    },
    ReloadWorkspace {
        session_id: SessionId,
        req_id: RequestId,
        active_schema: Option<String>,
    },
    FetchTablePage {
        session_id: SessionId,
        tab_id: TabId,
        req_id: RequestId,
        request: PageRequest,
    },
    DescribeTable {
        session_id: SessionId,
        tab_id: TabId,
        req_id: RequestId,
        schema: String,
        table: String,
    },
    ExecuteSql {
        session_id: SessionId,
        tab_id: TabId,
        req_id: RequestId,
        connection_id: ConnectionId,
        connection_name: String,
        database: Option<String>,
        statements: Vec<String>,
        executed_sql_text: String,
        row_limit: Option<usize>,
    },
    CancelQuery {
        session_id: SessionId,
    },
    LoadQueryLibrary {
        session_id: SessionId,
        connection_id: ConnectionId,
    },
    SaveQuery {
        session_id: SessionId,
        tab_id: Option<TabId>,
        session_connection_id: ConnectionId,
        connection_id: Option<ConnectionId>,
        id: Option<SavedQueryId>,
        folder: Option<String>,
        name: String,
        sql: String,
    },
    DeleteSavedQuery {
        session_id: SessionId,
        connection_id: ConnectionId,
        id: SavedQueryId,
    },
    ClearQueryHistory {
        session_id: SessionId,
        connection_id: Option<ConnectionId>,
    },
    Disconnect {
        session_id: SessionId,
    },
    TestConnection {
        req_id: RequestId,
        profile: ConnectionProfile,
        password_override: Option<String>,
    },
    SaveCredential {
        service: String,
        account: String,
        secret: String,
    },
    DeleteCredential {
        service: String,
    },
    LoadCredentialForEdit {
        req_id: RequestId,
        connection_id: ConnectionId,
        service: String,
    },
    CommitStagedChanges {
        session_id: SessionId,
        tab_id: TabId,
        req_id: RequestId,
        statements: Vec<String>,
    },
    ExplainSql {
        session_id: SessionId,
        tab_id: TabId,
        req_id: RequestId,
        statement: String,
        analyze: bool,
    },
    FetchFkPeek {
        session_id: SessionId,
        tab_id: TabId,
        req_id: RequestId,
        fk: ForeignKeyRef,
        value: CellValue,
        source_col: ColumnMeta,
    },
}

/// Asynchronous results sent from the database worker back to `AppState`.
#[derive(Debug)]
pub enum WorkerEvent {
    Connected {
        session_id: SessionId,
        req_id: RequestId,
        server_info: ServerInfo,
        databases: Vec<DatabaseInfo>,
        active_database: Option<String>,
        schemas: Vec<SchemaInfo>,
        active_schema: Option<String>,
        objects: Vec<DbObject>,
        logs: Vec<ExecutedSql>,
    },
    ConnectFailed {
        session_id: SessionId,
        req_id: RequestId,
        error: DbError,
    },
    SchemasAndObjectsLoaded {
        session_id: SessionId,
        req_id: RequestId,
        databases: Vec<DatabaseInfo>,
        active_database: String,
        schemas: Vec<SchemaInfo>,
        active_schema: Option<String>,
        objects: Vec<DbObject>,
        logs: Vec<ExecutedSql>,
    },
    ObjectsLoaded {
        session_id: SessionId,
        req_id: RequestId,
        schema: String,
        objects: Vec<DbObject>,
        logs: Vec<ExecutedSql>,
    },
    ColumnsLoaded {
        session_id: SessionId,
        req_id: RequestId,
        schema: String,
        object: String,
        columns: Result<Vec<ColumnSummary>, DbError>,
        logs: Vec<ExecutedSql>,
    },
    TablePageLoaded {
        session_id: SessionId,
        tab_id: TabId,
        req_id: RequestId,
        result: Result<TablePage, DbError>,
        logs: Vec<ExecutedSql>,
    },
    TableStructureLoaded {
        session_id: SessionId,
        tab_id: TabId,
        req_id: RequestId,
        result: Result<TableStructure, DbError>,
        logs: Vec<ExecutedSql>,
    },
    QueryTick {
        session_id: SessionId,
        tab_id: TabId,
        req_id: RequestId,
        elapsed_ms: u64,
    },
    QueryCompleted {
        session_id: SessionId,
        tab_id: TabId,
        req_id: RequestId,
        result: Result<QueryBatchResult, DbError>,
        logs: Vec<ExecutedSql>,
        recorded_history: Option<QueryHistoryEntry>,
    },
    QueryLibraryLoaded {
        session_id: SessionId,
        saved_queries: Vec<SavedQuery>,
        history: Vec<QueryHistoryEntry>,
    },
    SavedQueryMutated {
        session_id: SessionId,
        tab_id: Option<TabId>,
        saved: Option<SavedQuery>,
        saved_queries: Vec<SavedQuery>,
        error: Option<String>,
    },
    HistoryCleared {
        session_id: SessionId,
        history: Vec<QueryHistoryEntry>,
    },
    OperationFailed {
        session_id: SessionId,
        req_id: RequestId,
        error: DbError,
    },
    TestResult {
        req_id: RequestId,
        result: Result<ServerInfo, DbError>,
    },
    CredentialLoaded {
        req_id: RequestId,
        connection_id: ConnectionId,
        secret: Option<String>,
    },
    StagedChangesCommitted {
        session_id: SessionId,
        tab_id: TabId,
        req_id: RequestId,
        result: Result<TransactionOutcome, DbError>,
        logs: Vec<ExecutedSql>,
    },
    ExplainCompleted {
        session_id: SessionId,
        tab_id: TabId,
        req_id: RequestId,
        result: Result<Box<ExplainPlan>, DbError>,
        logs: Vec<ExecutedSql>,
    },
    FkPeekLoaded {
        session_id: SessionId,
        tab_id: TabId,
        req_id: RequestId,
        result: Result<FkPeekRow, DbError>,
        logs: Vec<ExecutedSql>,
    },
}

/// Owns the background Tokio runtime thread and wakes Rune whenever worker events arrive.
pub struct WorkerBridge {
    cmd_tx: tokio::sync::mpsc::UnboundedSender<WorkerCmd>,
    event_rx: mpsc::Receiver<WorkerEvent>,
    waker: Arc<std::sync::Mutex<Option<Waker>>>,
    runtime_handle: tokio::runtime::Handle,
    _thread: Option<std::thread::JoinHandle<()>>,
}

impl WorkerBridge {
    /// Spawns the background Tokio worker thread. Returns `None` if runtime initialization fails.
    pub fn spawn() -> Option<Self> {
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::channel();
        let (handle_tx, handle_rx) = std::sync::mpsc::sync_channel(1);
        let waker: Arc<std::sync::Mutex<Option<Waker>>> = Arc::new(std::sync::Mutex::new(None));
        let worker_waker = Arc::clone(&waker);

        let thread = std::thread::Builder::new()
            .name("tv-db-worker".to_owned())
            .spawn(move || {
                let Ok(rt) = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                else {
                    return;
                };
                if handle_tx.send(rt.handle().clone()).is_err() {
                    return;
                }
                rt.block_on(worker_loop(cmd_rx, event_tx, worker_waker));
            })
            .ok()?;

        let runtime_handle = handle_rx.recv().ok()?;

        Some(Self {
            cmd_tx,
            event_rx,
            waker,
            runtime_handle,
            _thread: Some(thread),
        })
    }

    pub fn runtime_handle(&self) -> tokio::runtime::Handle {
        self.runtime_handle.clone()
    }

    pub fn set_waker(&self, waker: Waker) {
        if let Ok(mut guard) = self.waker.lock() {
            *guard = Some(waker);
        }
    }

    pub fn send(&self, cmd: WorkerCmd) {
        drop(self.cmd_tx.send(cmd));
    }

    pub fn drain_events(&self) -> Vec<WorkerEvent> {
        let mut events = Vec::new();
        while let Ok(ev) = self.event_rx.try_recv() {
            events.push(ev);
        }
        events
    }
}

fn emit(tx: &mpsc::Sender<WorkerEvent>, waker: &Arc<std::sync::Mutex<Option<Waker>>>, event: WorkerEvent) {
    if tx.send(event).is_ok()
        && let Ok(guard) = waker.lock()
        && let Some(w) = guard.as_ref()
    {
        w();
    }
}

async fn resolve_password(
    profile: &ConnectionProfile,
    explicit: Option<String>,
) -> Option<String> {
    if let Some(pw) = explicit.filter(|s| !s.is_empty()) {
        return Some(pw);
    }
    if profile.password_mode != PasswordMode::Keychain {
        return None;
    }
    let service = profile.credential_service();
    tokio::task::spawn_blocking(move || {
        Credentials::system()
            .read(&service)
            .ok()
            .flatten()
            .and_then(|cred| String::from_utf8(cred.secret().to_vec()).ok())
            .filter(|s| !s.is_empty())
    })
    .await
    .ok()
    .flatten()
}

fn pick_default_schema(schemas: &[SchemaInfo], preferred: Option<&str>) -> Option<String> {
    if let Some(pref) = preferred
        && schemas.iter().any(|s| s.name == pref)
    {
        return Some(pref.to_owned());
    }
    if schemas.iter().any(|s| s.name == "public") {
        return Some("public".to_owned());
    }
    schemas
        .iter()
        .find(|s| !s.is_system)
        .or_else(|| schemas.first())
        .map(|s| s.name.clone())
}

async fn worker_loop(
    mut cmd_rx: tokio::sync::mpsc::UnboundedReceiver<WorkerCmd>,
    event_tx: mpsc::Sender<WorkerEvent>,
    waker: Arc<std::sync::Mutex<Option<Waker>>>,
) {
    let sessions: Arc<tokio::sync::Mutex<HashMap<SessionId, Arc<PostgresSession>>>> =
        Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let query_store: Option<Arc<QueryStore>> = QueryStore::open_default().ok().map(Arc::new);

    while let Some(cmd) = cmd_rx.recv().await {
        let sessions = Arc::clone(&sessions);
        let query_store = query_store.clone();
        let event_tx = event_tx.clone();
        let waker = Arc::clone(&waker);

        tokio::spawn(async move {
            handle_cmd(cmd, sessions, query_store, event_tx, waker).await;
        });
    }
}

async fn handle_cmd(
    cmd: WorkerCmd,
    sessions: Arc<tokio::sync::Mutex<HashMap<SessionId, Arc<PostgresSession>>>>,
    query_store: Option<Arc<QueryStore>>,
    event_tx: mpsc::Sender<WorkerEvent>,
    waker: Arc<std::sync::Mutex<Option<Waker>>>,
) {
    match cmd {
        WorkerCmd::Connect {
            session_id,
            req_id,
            profile,
            prompt_password,
            database,
        } => {
            let explicit_db = database
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .or_else(|| profile.database.as_deref().map(str::trim).filter(|s| !s.is_empty()))
                .map(str::to_owned);

            let pw = resolve_password(&profile, prompt_password).await;
            let connected = PostgresSession::connect(&profile, pw.as_deref(), explicit_db.as_deref()).await;
            let (session, mut logs) = match connected {
                Ok(ok) => ok,
                Err(error) => {
                    emit(&event_tx, &waker, WorkerEvent::ConnectFailed { session_id, req_id, error });
                    return;
                }
            };
            let session = Arc::new(session);
            let server_info = session.server_info().clone();

            let (databases, db_log) = match session.list_databases().await {
                Ok(res) => res,
                Err(error) => {
                    emit(&event_tx, &waker, WorkerEvent::ConnectFailed { session_id, req_id, error });
                    return;
                }
            };
            logs.push(db_log);

            let (schemas, active_schema, objects) = if explicit_db.is_some() {
                match session.list_schemas().await {
                    Ok((schemas, schema_log)) => {
                        logs.push(schema_log);
                        let chosen = pick_default_schema(&schemas, None);
                        let objs = if let Some(ref schema_name) = chosen {
                            match session.list_objects(schema_name).await {
                                Ok((objs, obj_log)) => {
                                    logs.push(obj_log);
                                    objs
                                }
                                Err(error) => {
                                    emit(&event_tx, &waker, WorkerEvent::ConnectFailed { session_id, req_id, error });
                                    return;
                                }
                            }
                        } else {
                            Vec::new()
                        };
                        (schemas, chosen, objs)
                    }
                    Err(error) => {
                        emit(&event_tx, &waker, WorkerEvent::ConnectFailed { session_id, req_id, error });
                        return;
                    }
                }
            } else {
                (Vec::new(), None, Vec::new())
            };

            sessions.lock().await.insert(session_id, session);
            emit(
                &event_tx,
                &waker,
                WorkerEvent::Connected {
                    session_id,
                    req_id,
                    server_info,
                    databases,
                    active_database: explicit_db,
                    schemas,
                    active_schema,
                    objects,
                    logs,
                },
            );
        }

        WorkerCmd::SelectDatabase {
            session_id,
            req_id,
            profile,
            prompt_password,
            database,
        } => {
            let pw = resolve_password(&profile, prompt_password).await;
            let connected = PostgresSession::connect(&profile, pw.as_deref(), Some(&database)).await;
            let (session, mut logs) = match connected {
                Ok(ok) => ok,
                Err(error) => {
                    emit(&event_tx, &waker, WorkerEvent::OperationFailed { session_id, req_id, error });
                    return;
                }
            };
            let session = Arc::new(session);
            let databases = match session.list_databases().await {
                Ok((dbs, log)) => {
                    logs.push(log);
                    dbs
                }
                Err(error) => {
                    emit(&event_tx, &waker, WorkerEvent::OperationFailed { session_id, req_id, error });
                    return;
                }
            };
            let (schemas, schema_log) = match session.list_schemas().await {
                Ok(res) => res,
                Err(error) => {
                    emit(&event_tx, &waker, WorkerEvent::OperationFailed { session_id, req_id, error });
                    return;
                }
            };
            logs.push(schema_log);

            let active_schema = pick_default_schema(&schemas, None);
            let objects = if let Some(ref schema_name) = active_schema {
                match session.list_objects(schema_name).await {
                    Ok((objs, obj_log)) => {
                        logs.push(obj_log);
                        objs
                    }
                    Err(error) => {
                        emit(&event_tx, &waker, WorkerEvent::OperationFailed { session_id, req_id, error });
                        return;
                    }
                }
            } else {
                Vec::new()
            };

            sessions.lock().await.insert(session_id, session);
            emit(
                &event_tx,
                &waker,
                WorkerEvent::SchemasAndObjectsLoaded {
                    session_id,
                    req_id,
                    databases,
                    active_database: database,
                    schemas,
                    active_schema,
                    objects,
                    logs,
                },
            );
        }

        WorkerCmd::SelectSchema {
            session_id,
            req_id,
            schema,
        } => {
            let Some(session) = sessions.lock().await.get(&session_id).cloned() else {
                emit(
                    &event_tx,
                    &waker,
                    WorkerEvent::OperationFailed {
                        session_id,
                        req_id,
                        error: DbError::Disconnected,
                    },
                );
                return;
            };
            match session.list_objects(&schema).await {
                Ok((objects, log)) => emit(
                    &event_tx,
                    &waker,
                    WorkerEvent::ObjectsLoaded {
                        session_id,
                        req_id,
                        schema,
                        objects,
                        logs: vec![log],
                    },
                ),
                Err(error) => emit(
                    &event_tx,
                    &waker,
                    WorkerEvent::OperationFailed { session_id, req_id, error },
                ),
            }
        }

        WorkerCmd::LoadColumns {
            session_id,
            req_id,
            schema,
            object,
        } => {
            let Some(session) = sessions.lock().await.get(&session_id).cloned() else {
                return;
            };
            match session.list_columns(&schema, &object).await {
                Ok((columns, log)) => emit(
                    &event_tx,
                    &waker,
                    WorkerEvent::ColumnsLoaded {
                        session_id,
                        req_id,
                        schema,
                        object,
                        columns: Ok(columns),
                        logs: vec![log],
                    },
                ),
                Err(error) => emit(
                    &event_tx,
                    &waker,
                    WorkerEvent::ColumnsLoaded {
                        session_id,
                        req_id,
                        schema,
                        object,
                        columns: Err(error),
                        logs: Vec::new(),
                    },
                ),
            }
        }

        WorkerCmd::ReloadWorkspace {
            session_id,
            req_id,
            active_schema,
        } => {
            let Some(session) = sessions.lock().await.get(&session_id).cloned() else {
                emit(
                    &event_tx,
                    &waker,
                    WorkerEvent::OperationFailed {
                        session_id,
                        req_id,
                        error: DbError::Disconnected,
                    },
                );
                return;
            };
            let mut logs = Vec::new();
            let databases = match session.list_databases().await {
                Ok((dbs, log)) => {
                    logs.push(log);
                    dbs
                }
                Err(error) => {
                    emit(&event_tx, &waker, WorkerEvent::OperationFailed { session_id, req_id, error });
                    return;
                }
            };
            let (schemas, schema_log) = match session.list_schemas().await {
                Ok(res) => res,
                Err(error) => {
                    emit(&event_tx, &waker, WorkerEvent::OperationFailed { session_id, req_id, error });
                    return;
                }
            };
            logs.push(schema_log);
            let chosen = pick_default_schema(&schemas, active_schema.as_deref());
            let objects = if let Some(ref schema_name) = chosen {
                match session.list_objects(schema_name).await {
                    Ok((objs, obj_log)) => {
                        logs.push(obj_log);
                        objs
                    }
                    Err(error) => {
                        emit(&event_tx, &waker, WorkerEvent::OperationFailed { session_id, req_id, error });
                        return;
                    }
                }
            } else {
                Vec::new()
            };
            let active_database = session.server_info().current_database.clone();
            emit(
                &event_tx,
                &waker,
                WorkerEvent::SchemasAndObjectsLoaded {
                    session_id,
                    req_id,
                    databases,
                    active_database,
                    schemas,
                    active_schema: chosen,
                    objects,
                    logs,
                },
            );
        }

        WorkerCmd::FetchTablePage {
            session_id,
            tab_id,
            req_id,
            request,
        } => {
            let Some(session) = sessions.lock().await.get(&session_id).cloned() else {
                emit(
                    &event_tx,
                    &waker,
                    WorkerEvent::TablePageLoaded {
                        session_id,
                        tab_id,
                        req_id,
                        result: Err(DbError::Disconnected),
                        logs: Vec::new(),
                    },
                );
                return;
            };
            match session.fetch_table_page(&request).await {
                Ok((page, logs)) => emit(
                    &event_tx,
                    &waker,
                    WorkerEvent::TablePageLoaded {
                        session_id,
                        tab_id,
                        req_id,
                        result: Ok(page),
                        logs,
                    },
                ),
                Err(error) => {
                    let failed_sql = format_failed_page_sql(&request);
                    let logs = vec![ExecutedSql {
                        sql: failed_sql,
                        duration_ms: 0,
                        error: Some(error.to_string()),
                    }];
                    emit(
                        &event_tx,
                        &waker,
                        WorkerEvent::TablePageLoaded {
                            session_id,
                            tab_id,
                            req_id,
                            result: Err(error),
                            logs,
                        },
                    );
                }
            }
        }

        WorkerCmd::DescribeTable {
            session_id,
            tab_id,
            req_id,
            schema,
            table,
        } => {
            let Some(session) = sessions.lock().await.get(&session_id).cloned() else {
                emit(
                    &event_tx,
                    &waker,
                    WorkerEvent::TableStructureLoaded {
                        session_id,
                        tab_id,
                        req_id,
                        result: Err(DbError::Disconnected),
                        logs: Vec::new(),
                    },
                );
                return;
            };
            match session.describe_table(&schema, &table).await {
                Ok((structure, logs)) => emit(
                    &event_tx,
                    &waker,
                    WorkerEvent::TableStructureLoaded {
                        session_id,
                        tab_id,
                        req_id,
                        result: Ok(structure),
                        logs,
                    },
                ),
                Err(error) => emit(
                    &event_tx,
                    &waker,
                    WorkerEvent::TableStructureLoaded {
                        session_id,
                        tab_id,
                        req_id,
                        result: Err(error),
                        logs: Vec::new(),
                    },
                ),
            }
        }

        WorkerCmd::ExecuteSql {
            session_id,
            tab_id,
            req_id,
            connection_id,
            connection_name,
            database,
            statements,
            executed_sql_text,
            row_limit,
        } => {
            let Some(session) = sessions.lock().await.get(&session_id).cloned() else {
                emit(
                    &event_tx,
                    &waker,
                    WorkerEvent::QueryCompleted {
                        session_id,
                        tab_id,
                        req_id,
                        result: Err(DbError::Disconnected),
                        logs: Vec::new(),
                        recorded_history: None,
                    },
                );
                return;
            };

            let start = Instant::now();
            let mut ticker = tokio::time::interval(Duration::from_millis(100));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            ticker.tick().await;

            let exec_fut = session.execute_sql(&statements, row_limit);
            tokio::pin!(exec_fut);

            let outcome = loop {
                tokio::select! {
                    res = &mut exec_fut => break res,
                    _ = ticker.tick() => {
                        emit(
                            &event_tx,
                            &waker,
                            WorkerEvent::QueryTick {
                                session_id,
                                tab_id,
                                req_id,
                                elapsed_ms: start.elapsed().as_millis() as u64,
                            },
                        );
                    }
                }
            };

            let (result, logs) = match outcome {
                Ok((batch, logs)) => (Ok(batch), logs),
                Err(error) => (Err(error), Vec::new()),
            };

            let recorded_history = if let Some(store) = query_store
                && !executed_sql_text.trim().is_empty()
            {
                let (duration_ms, rows_count, error) = match &result {
                    Ok(batch) => {
                        let err = if batch.cancelled {
                            Some("Query cancelled".to_owned())
                        } else {
                            batch.results.iter().find_map(|r| r.error.clone())
                        };
                        let rows = if batch.results.iter().any(|r| r.is_tabular()) {
                            Some(batch.results.iter().map(|r| r.rows.len() as u64).sum())
                        } else {
                            batch
                                .results
                                .iter()
                                .filter_map(|r| r.rows_affected)
                                .reduce(|a, b| a + b)
                        };
                        (batch.total_elapsed_ms, rows, err)
                    }
                    Err(err) => (start.elapsed().as_millis() as u64, None, Some(err.to_string())),
                };
                let entry = NewHistoryEntry {
                    connection_id: Some(connection_id),
                    connection_name,
                    database_name: database,
                    sql: executed_sql_text,
                    duration_ms,
                    rows_count,
                    error,
                };
                tokio::task::spawn_blocking(move || store.record_history(&entry).ok())
                    .await
                    .ok()
                    .flatten()
            } else {
                None
            };

            emit(
                &event_tx,
                &waker,
                WorkerEvent::QueryCompleted {
                    session_id,
                    tab_id,
                    req_id,
                    result,
                    logs,
                    recorded_history,
                },
            );
        }

        WorkerCmd::CancelQuery { session_id } => {
            let session = sessions.lock().await.get(&session_id).cloned();
            if let Some(session) = session {
                drop(session.cancel_running_query().await);
            }
        }

        WorkerCmd::LoadQueryLibrary {
            session_id,
            connection_id,
        } => {
            let (saved_queries, history) = if let Some(store) = query_store {
                tokio::task::spawn_blocking(move || {
                    let saved = store.list_saved_queries(Some(connection_id), None).unwrap_or_default();
                    let hist = store
                        .list_history(Some(connection_id), None, QUERY_HISTORY_LIMIT)
                        .unwrap_or_default();
                    (saved, hist)
                })
                .await
                .unwrap_or_default()
            } else {
                (Vec::new(), Vec::new())
            };
            emit(
                &event_tx,
                &waker,
                WorkerEvent::QueryLibraryLoaded {
                    session_id,
                    saved_queries,
                    history,
                },
            );
        }

        WorkerCmd::SaveQuery {
            session_id,
            tab_id,
            session_connection_id,
            connection_id,
            id,
            folder,
            name,
            sql,
        } => {
            let saved_query = SavedQuery {
                id: id.unwrap_or_default(),
                connection_id,
                folder,
                name,
                sql,
                updated_at: chrono::Utc::now(),
            };
            let (saved, saved_queries, error) = if let Some(store) = query_store {
                let query_clone = saved_query.clone();
                tokio::task::spawn_blocking(move || {
                    let save_res = store.upsert_saved_query(&query_clone);
                    let list = store
                        .list_saved_queries(Some(session_connection_id), None)
                        .unwrap_or_default();
                    match save_res {
                        Ok(()) => (Some(query_clone), list, None),
                        Err(e) => (None, list, Some(e.to_string())),
                    }
                })
                .await
                .unwrap_or_else(|e| (None, Vec::new(), Some(e.to_string())))
            } else {
                (
                    None,
                    Vec::new(),
                    Some("Local query store is unavailable".to_owned()),
                )
            };
            emit(
                &event_tx,
                &waker,
                WorkerEvent::SavedQueryMutated {
                    session_id,
                    tab_id,
                    saved,
                    saved_queries,
                    error,
                },
            );
        }

        WorkerCmd::DeleteSavedQuery {
            session_id,
            connection_id,
            id,
        } => {
            let (saved_queries, error) = if let Some(store) = query_store {
                tokio::task::spawn_blocking(move || {
                    let del_res = store.delete_saved_query(id);
                    let list = store.list_saved_queries(Some(connection_id), None).unwrap_or_default();
                    match del_res {
                        Ok(()) => (list, None),
                        Err(e) => (list, Some(e.to_string())),
                    }
                })
                .await
                .unwrap_or_else(|e| (Vec::new(), Some(e.to_string())))
            } else {
                (Vec::new(), Some("Local query store is unavailable".to_owned()))
            };
            emit(
                &event_tx,
                &waker,
                WorkerEvent::SavedQueryMutated {
                    session_id,
                    tab_id: None,
                    saved: None,
                    saved_queries,
                    error,
                },
            );
        }

        WorkerCmd::ClearQueryHistory {
            session_id,
            connection_id,
        } => {
            let history = if let Some(store) = query_store {
                tokio::task::spawn_blocking(move || {
                    drop(store.clear_history(connection_id));
                    Vec::new()
                })
                .await
                .unwrap_or_default()
            } else {
                Vec::new()
            };
            emit(
                &event_tx,
                &waker,
                WorkerEvent::HistoryCleared { session_id, history },
            );
        }

        WorkerCmd::Disconnect { session_id } => {
            sessions.lock().await.remove(&session_id);
        }

        WorkerCmd::TestConnection {
            req_id,
            profile,
            password_override,
        } => {
            let pw = resolve_password(&profile, password_override).await;
            let result = PostgresSession::test_connection(&profile, pw.as_deref()).await;
            emit(&event_tx, &waker, WorkerEvent::TestResult { req_id, result });
        }

        WorkerCmd::SaveCredential {
            service,
            account,
            secret,
        } => {
            drop(
                tokio::task::spawn_blocking(move || {
                    if secret.is_empty() {
                        drop(Credentials::system().delete(&service));
                    } else {
                        drop(Credentials::system().write(&service, &account, secret.as_bytes()));
                    }
                })
                .await,
            );
        }

        WorkerCmd::DeleteCredential { service } => {
            drop(
                tokio::task::spawn_blocking(move || {
                    drop(Credentials::system().delete(&service));
                })
                .await,
            );
        }

        WorkerCmd::LoadCredentialForEdit {
            req_id,
            connection_id,
            service,
        } => {
            let secret = tokio::task::spawn_blocking(move || {
                Credentials::system()
                    .read(&service)
                    .ok()
                    .flatten()
                    .and_then(|c| String::from_utf8(c.secret().to_vec()).ok())
            })
            .await
            .ok()
            .flatten();
            emit(
                &event_tx,
                &waker,
                WorkerEvent::CredentialLoaded {
                    req_id,
                    connection_id,
                    secret,
                },
            );
        }

        WorkerCmd::CommitStagedChanges {
            session_id,
            tab_id,
            req_id,
            statements,
        } => {
            let Some(session) = sessions.lock().await.get(&session_id).cloned() else {
                emit(
                    &event_tx,
                    &waker,
                    WorkerEvent::StagedChangesCommitted {
                        session_id,
                        tab_id,
                        req_id,
                        result: Err(DbError::Disconnected),
                        logs: Vec::new(),
                    },
                );
                return;
            };
            match session.execute_transaction(&statements).await {
                Ok((outcome, logs)) => emit(
                    &event_tx,
                    &waker,
                    WorkerEvent::StagedChangesCommitted {
                        session_id,
                        tab_id,
                        req_id,
                        result: Ok(outcome),
                        logs,
                    },
                ),
                Err(error) => {
                    let logs = vec![ExecutedSql {
                        sql: statements.join("\n"),
                        duration_ms: 0,
                        error: Some(error.to_string()),
                    }];
                    emit(
                        &event_tx,
                        &waker,
                        WorkerEvent::StagedChangesCommitted {
                            session_id,
                            tab_id,
                            req_id,
                            result: Err(error),
                            logs,
                        },
                    );
                }
            }
        }

        WorkerCmd::ExplainSql {
            session_id,
            tab_id,
            req_id,
            statement,
            analyze,
        } => {
            let Some(session) = sessions.lock().await.get(&session_id).cloned() else {
                emit(
                    &event_tx,
                    &waker,
                    WorkerEvent::ExplainCompleted {
                        session_id,
                        tab_id,
                        req_id,
                        result: Err(DbError::Disconnected),
                        logs: Vec::new(),
                    },
                );
                return;
            };
            match session.explain_sql(&statement, analyze).await {
                Ok((plan, log)) => emit(
                    &event_tx,
                    &waker,
                    WorkerEvent::ExplainCompleted {
                        session_id,
                        tab_id,
                        req_id,
                        result: Ok(Box::new(plan)),
                        logs: vec![log],
                    },
                ),
                Err(error) => emit(
                    &event_tx,
                    &waker,
                    WorkerEvent::ExplainCompleted {
                        session_id,
                        tab_id,
                        req_id,
                        result: Err(error),
                        logs: Vec::new(),
                    },
                ),
            }
        }

        WorkerCmd::FetchFkPeek {
            session_id,
            tab_id,
            req_id,
            fk,
            value,
            source_col,
        } => {
            let Some(session) = sessions.lock().await.get(&session_id).cloned() else {
                emit(
                    &event_tx,
                    &waker,
                    WorkerEvent::FkPeekLoaded {
                        session_id,
                        tab_id,
                        req_id,
                        result: Err(DbError::Disconnected),
                        logs: Vec::new(),
                    },
                );
                return;
            };
            match session.fetch_fk_row(&fk, &value, &source_col).await {
                Ok((peek, log)) => emit(
                    &event_tx,
                    &waker,
                    WorkerEvent::FkPeekLoaded {
                        session_id,
                        tab_id,
                        req_id,
                        result: Ok(peek),
                        logs: vec![log],
                    },
                ),
                Err(error) => emit(
                    &event_tx,
                    &waker,
                    WorkerEvent::FkPeekLoaded {
                        session_id,
                        tab_id,
                        req_id,
                        result: Err(error),
                        logs: Vec::new(),
                    },
                ),
            }
        }
    }
}

fn format_failed_page_sql(req: &PageRequest) -> String {
    let mut sql = format!(
        "SELECT * FROM {}.{}",
        quote_ident(&req.schema),
        quote_ident(&req.table)
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
    sql.push_str(&format!(" LIMIT {} OFFSET {}", req.limit, req.offset));
    sql
}
