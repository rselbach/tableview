use std::collections::HashMap;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use rune::Waker;
use rune::credentials::Credentials;
use tv_core::engine::{connect_session, test_connection};
use tv_core::{
    CellValue, ColumnMeta, ColumnSummary, ConnectionId, ConnectionProfile, DatabaseInfo, DbEngine, DbError, DbObject,
    DbSession, ExecutedSql, ExplainPlan, FkPeekRow, ForeignKeyRef, MysqlQuoteMode, NewHistoryEntry, PageRequest,
    PasswordMode, PostgresSessionMode, QueryBatchResult, QueryHistoryEntry, QueryStore, RequestId, SavedQuery,
    SavedQueryId, SchemaInfo, ServerInfo, SessionId, SqlSessionMode, TabId, TablePage, TableStructure,
    TransactionOutcome, batch_sets_credentials_with_session_mode, trim_sql_whitespace_for_engine,
};

const QUERY_HISTORY_LIMIT: usize = 200;

/// Future returned by [`SessionConnector`] when establishing a database session.
pub type ConnectSessionFuture = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<(Arc<dyn DbSession>, Vec<ExecutedSql>), DbError>> + Send>,
>;

/// Factory closure used by [`WorkerBridge`] to open or re-open database sessions.
pub type SessionConnector =
    Arc<dyn Fn(ConnectionProfile, Option<String>, Option<String>) -> ConnectSessionFuture + Send + Sync>;

fn default_session_connector() -> SessionConnector {
    Arc::new(|profile, password, database| {
        Box::pin(async move {
            connect_session(&profile, password.as_deref(), database.as_deref()).await
        })
    })
}

/// Commands dispatched from the UI thread to the background database worker.
#[derive(Debug)]
pub enum WorkerCmd {
    Connect {
        session_id: SessionId,
        req_id: RequestId,
        profile: ConnectionProfile,
        prompt_password: Option<String>,
        database: Option<String>,
        preferred_schema: Option<String>,
    },
    UpdateSessionProfile {
        session_id: SessionId,
        profile: ConnectionProfile,
        password_update: Option<Option<String>>,
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
        connect_req_id: RequestId,
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
        connect_req_id: RequestId,
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
        engine: DbEngine,
        connect_req_id: RequestId,
        connection_id: ConnectionId,
        connection_name: String,
        database: Option<String>,
        statements: Vec<String>,
        executed_sql_text: String,
        row_limit: Option<usize>,
        record_history: bool,
        expected_sql_mode: SqlSessionMode,
    },
    CancelQuery {
        session_id: SessionId,
        req_id: RequestId,
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
        req_id: RequestId,
        connection_id: ConnectionId,
        service: String,
        account: String,
        secret: String,
    },
    DeleteCredential {
        req_id: RequestId,
        connection_id: ConnectionId,
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
        connect_req_id: RequestId,
        statements: Vec<String>,
    },
    ExplainSql {
        session_id: SessionId,
        tab_id: TabId,
        req_id: RequestId,
        connect_req_id: RequestId,
        statement: String,
        analyze: bool,
        expected_sql_mode: SqlSessionMode,
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
        mysql_quote_mode: Option<MysqlQuoteMode>,
        postgres_session_mode: Option<PostgresSessionMode>,
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
    SessionDisconnected {
        session_id: SessionId,
        connect_req_id: RequestId,
        error: DbError,
    },
    SchemasAndObjectsLoaded {
        session_id: SessionId,
        req_id: RequestId,
        is_db_switch: bool,
        server_version: Option<String>,
        mysql_quote_mode: Option<MysqlQuoteMode>,
        postgres_session_mode: Option<PostgresSessionMode>,
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
    SessionModeUpdated {
        session_id: SessionId,
        connect_req_id: RequestId,
        mode_seq: u64,
        mysql_quote_mode: Option<MysqlQuoteMode>,
        postgres_session_mode: Option<PostgresSessionMode>,
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
    CredentialSaved {
        req_id: RequestId,
        connection_id: ConnectionId,
        result: Result<(), String>,
    },
    CredentialDeleted {
        req_id: RequestId,
        connection_id: ConnectionId,
        result: Result<(), String>,
    },
    CredentialLoaded {
        req_id: RequestId,
        connection_id: ConnectionId,
        result: Result<Option<String>, String>,
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

/// Abstraction over OS credential storage used by [`WorkerBridge`] so tests can inject mock backends.
pub trait CredentialBackend: Send + Sync {
    fn read(&self, service: &str) -> Result<Option<String>, String>;
    fn write(&self, service: &str, account: &str, secret: &str) -> Result<(), String>;
    fn delete(&self, service: &str) -> Result<(), String>;
}

fn read_credentials_utf8(store: &Credentials, service: &str) -> Result<Option<String>, String> {
    let cred = store.read(service).map_err(|err| err.to_string())?;
    let Some(cred) = cred else {
        return Ok(None);
    };
    String::from_utf8(cred.secret().to_vec())
        .map(Some)
        .map_err(|_| "stored credential is not valid UTF-8".to_owned())
}

struct SystemCredentialBackend;

impl CredentialBackend for SystemCredentialBackend {
    fn read(&self, service: &str) -> Result<Option<String>, String> {
        read_credentials_utf8(&Credentials::system(), service)
    }

    fn write(&self, service: &str, account: &str, secret: &str) -> Result<(), String> {
        Credentials::system()
            .write(service, account, secret.as_bytes())
            .map_err(|err| err.to_string())
    }

    fn delete(&self, service: &str) -> Result<(), String> {
        Credentials::system()
            .delete(service)
            .map_err(|err| err.to_string())
    }
}

struct MemoryCredentialBackend(Credentials);

impl MemoryCredentialBackend {
    fn new() -> Self {
        Self(Credentials::memory())
    }
}

impl CredentialBackend for MemoryCredentialBackend {
    fn read(&self, service: &str) -> Result<Option<String>, String> {
        read_credentials_utf8(&self.0, service)
    }

    fn write(&self, service: &str, account: &str, secret: &str) -> Result<(), String> {
        self.0
            .write(service, account, secret.as_bytes())
            .map_err(|err| err.to_string())
    }

    fn delete(&self, service: &str) -> Result<(), String> {
        self.0.delete(service).map_err(|err| err.to_string())
    }
}

enum CredentialTask {
    Read {
        service: String,
        reply: tokio::sync::oneshot::Sender<Result<Option<String>, String>>,
    },
    Write {
        service: String,
        account: String,
        secret: String,
        reply: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    Delete {
        service: String,
        reply: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
}

enum PendingCredentialRx {
    Read(tokio::sync::oneshot::Receiver<Result<Option<String>, String>>),
    Mutate(tokio::sync::oneshot::Receiver<Result<(), String>>),
}

#[derive(Clone)]
struct CredentialRunner {
    backend: Arc<dyn CredentialBackend>,
    queues: Arc<std::sync::Mutex<HashMap<String, tokio::sync::mpsc::UnboundedSender<CredentialTask>>>>,
}

impl CredentialRunner {
    fn new(backend: Arc<dyn CredentialBackend>) -> Self {
        Self {
            backend,
            queues: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    fn enqueue_task(&self, service: &str, mut task: CredentialTask) {
        let Ok(mut map) = self.queues.lock() else {
            return;
        };
        if let Some(tx) = map.get(service) {
            match tx.send(task) {
                Ok(()) => return,
                Err(tokio::sync::mpsc::error::SendError(unsent)) => {
                    task = unsent;
                }
            }
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<CredentialTask>();
        drop(tx.send(task));
        map.insert(service.to_owned(), tx);
        let backend = Arc::clone(&self.backend);
        let queues = Arc::clone(&self.queues);
        let service_key = service.to_owned();
        tokio::spawn(async move {
            loop {
                let next_task = {
                    let Ok(mut guard) = queues.lock() else {
                        break;
                    };
                    if let Ok(t) = rx.try_recv() {
                        t
                    } else {
                        guard.remove(&service_key);
                        drop(rx);
                        break;
                    }
                };
                let backend_ref = Arc::clone(&backend);
                match next_task {
                    CredentialTask::Read { service, reply } => {
                        let res =
                            tokio::task::spawn_blocking(move || backend_ref.read(&service))
                                .await
                                .unwrap_or_else(|e| {
                                    Err(format!("credential read task panicked: {e}"))
                                });
                        drop(reply.send(res));
                    }
                    CredentialTask::Write {
                        service,
                        account,
                        secret,
                        reply,
                    } => {
                        let res = tokio::task::spawn_blocking(move || {
                            backend_ref.write(&service, &account, &secret)
                        })
                        .await
                        .unwrap_or_else(|e| Err(format!("credential write task panicked: {e}")));
                        drop(reply.send(res));
                    }
                    CredentialTask::Delete { service, reply } => {
                        let res =
                            tokio::task::spawn_blocking(move || backend_ref.delete(&service))
                                .await
                                .unwrap_or_else(|e| {
                                    Err(format!("credential delete task panicked: {e}"))
                                });
                        drop(reply.send(res));
                    }
                }
            }
        });
    }

    fn enqueue_read(
        &self,
        service: &str,
    ) -> tokio::sync::oneshot::Receiver<Result<Option<String>, String>> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.enqueue_task(
            service,
            CredentialTask::Read {
                service: service.to_owned(),
                reply,
            },
        );
        rx
    }

    fn enqueue_write(
        &self,
        service: &str,
        account: &str,
        secret: &str,
    ) -> tokio::sync::oneshot::Receiver<Result<(), String>> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.enqueue_task(
            service,
            CredentialTask::Write {
                service: service.to_owned(),
                account: account.to_owned(),
                secret: secret.to_owned(),
                reply,
            },
        );
        rx
    }

    fn enqueue_delete(&self, service: &str) -> tokio::sync::oneshot::Receiver<Result<(), String>> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.enqueue_task(
            service,
            CredentialTask::Delete {
                service: service.to_owned(),
                reply,
            },
        );
        rx
    }
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
        Self::spawn_inner(
            default_session_connector(),
            false,
            Arc::new(SystemCredentialBackend),
        )
    }

    /// Spawns the background worker with a custom session connector and isolated in-memory stores.
    pub fn spawn_with_connector(connector: SessionConnector) -> Option<Self> {
        Self::spawn_inner(
            connector,
            true,
            Arc::new(MemoryCredentialBackend::new()),
        )
    }

    /// Spawns the background worker with custom session connector and credential backend for tests.
    pub fn spawn_with_connector_and_credentials(
        connector: SessionConnector,
        credentials: Arc<dyn CredentialBackend>,
    ) -> Option<Self> {
        Self::spawn_inner(connector, true, credentials)
    }

    fn spawn_inner(
        connector: SessionConnector,
        in_memory_history: bool,
        credential_backend: Arc<dyn CredentialBackend>,
    ) -> Option<Self> {
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
                rt.block_on(worker_loop(
                    cmd_rx,
                    event_tx,
                    worker_waker,
                    connector,
                    CredentialRunner::new(credential_backend),
                    in_memory_history,
                ));
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

async fn resolve_password_with_rx(
    profile: &ConnectionProfile,
    explicit: Option<String>,
    cred_rx: Option<PendingCredentialRx>,
) -> Result<Option<String>, String> {
    if let Some(pw) = explicit.filter(|s| !s.is_empty()) {
        return Ok(Some(pw));
    }
    if profile.password_mode != PasswordMode::Keychain {
        return Ok(None);
    }
    let Some(PendingCredentialRx::Read(rx)) = cred_rx else {
        return Ok(None);
    };
    let secret = rx
        .await
        .map_err(|_| "credential worker channel closed".to_owned())??;
    Ok(secret.filter(|s| !s.is_empty()))
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
    if schemas.iter().any(|s| s.name == "main") {
        return Some("main".to_owned());
    }
    schemas
        .iter()
        .find(|s| !s.is_system)
        .or_else(|| schemas.first())
        .map(|s| s.name.clone())
}

#[derive(Default)]
struct ReconnectGate {
    last_failure: Option<(Instant, DbError)>,
}

struct SessionEntry {
    connect_req_id: RequestId,
    mode_seq: u64,
    session: Arc<dyn DbSession>,
    profile: ConnectionProfile,
    password: Option<String>,
    database: Option<String>,
    reconnect_gate: Arc<tokio::sync::Mutex<ReconnectGate>>,
}

type SessionMap = Arc<tokio::sync::Mutex<HashMap<SessionId, SessionEntry>>>;
type SessionEpochMap = Arc<std::sync::Mutex<HashMap<SessionId, u64>>>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum LivenessCheck {
    Fast,
    AlwaysPing,
}

enum SessionCallError {
    Operation { engine: DbEngine, error: DbError },
    Disconnected { connect_req_id: Option<RequestId>, cause: DbError },
}

struct QueryCancelEntry {
    tx: tokio::sync::watch::Sender<bool>,
    flag: Arc<std::sync::atomic::AtomicBool>,
    session: Option<Arc<dyn DbSession>>,
}
type QueryCancelMap = Arc<std::sync::Mutex<HashMap<RequestId, QueryCancelEntry>>>;
type QueryCancelHandle = (
    tokio::sync::watch::Receiver<bool>,
    Arc<std::sync::atomic::AtomicBool>,
);

fn is_current_lifecycle_epoch(epochs: &SessionEpochMap, session_id: SessionId, expected: Option<u64>) -> bool {
    let Some(expected) = expected else {
        return true;
    };
    epochs
        .lock()
        .ok()
        .and_then(|guard| guard.get(&session_id).copied())
        == Some(expected)
}

struct CancelRegistration {
    map: QueryCancelMap,
    req_id: RequestId,
}

impl Drop for CancelRegistration {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.map.lock() {
            guard.remove(&self.req_id);
        }
    }
}

fn supports_background_reconnect(engine: DbEngine) -> bool {
    matches!(engine, DbEngine::Postgres | DbEngine::Mysql)
}

fn unwrap_disconnect_cause(error: DbError) -> DbError {
    match error {
        DbError::DisconnectedBeforeDispatch(inner) => *inner,
        other => other,
    }
}

fn is_session_disconnect_err(session: &dyn DbSession, error: &DbError) -> bool {
    matches!(
        error,
        DbError::Disconnected | DbError::DisconnectedBeforeDispatch(_)
    ) || (session.is_disconnected()
        && matches!(
            error,
            DbError::AuthFailed(_) | DbError::ConnectionFailed(_) | DbError::TlsConfig(_)
        ))
}

fn emit_session_disconnected(
    tx: &mpsc::Sender<WorkerEvent>,
    waker: &Arc<std::sync::Mutex<Option<Waker>>>,
    session_id: SessionId,
    connect_req_id: Option<RequestId>,
    error: DbError,
) {
    if let Some(connect_req_id) = connect_req_id {
        emit(
            tx,
            waker,
            WorkerEvent::SessionDisconnected {
                session_id,
                connect_req_id,
                error: unwrap_disconnect_cause(error),
            },
        );
    }
}

async fn current_connect_req_id(
    sessions: &SessionMap,
    session_id: SessionId,
    expected_connect_req_id: RequestId,
) -> Option<RequestId> {
    let map = sessions.lock().await;
    map.get(&session_id)
        .filter(|entry| entry.connect_req_id == expected_connect_req_id)
        .map(|entry| entry.connect_req_id)
}

async fn emit_session_mode(
    sessions: &SessionMap,
    event_tx: &mpsc::Sender<WorkerEvent>,
    waker: &Arc<std::sync::Mutex<Option<Waker>>>,
    session_id: SessionId,
    expected_connect_req_id: Option<RequestId>,
) {
    let mut map = sessions.lock().await;
    let Some(entry) = map.get_mut(&session_id) else {
        return;
    };
    if expected_connect_req_id.is_some_and(|exp| entry.connect_req_id != exp) {
        return;
    }
    entry.mode_seq = entry.mode_seq.saturating_add(1);
    let mysql_quote_mode = entry.session.mysql_quote_mode();
    let postgres_session_mode = entry.session.postgres_session_mode();
    emit(
        event_tx,
        waker,
        WorkerEvent::SessionModeUpdated {
            session_id,
            connect_req_id: entry.connect_req_id,
            mode_seq: entry.mode_seq,
            mysql_quote_mode,
            postgres_session_mode,
        },
    );
}

async fn reconnect_session(
    sessions: &SessionMap,
    connector: &SessionConnector,
    credentials: &CredentialRunner,
    session_id: SessionId,
    stale_session: &Arc<dyn DbSession>,
    stale_connect_req_id: RequestId,
) -> Result<(Arc<dyn DbSession>, RequestId), (Option<RequestId>, DbError)> {
    let call_started = Instant::now();
    let (gate, profile, cached_pw, database) = {
        let map = sessions.lock().await;
        let Some(entry) = map.get(&session_id) else {
            return Err((None, DbError::Disconnected));
        };
        if entry.connect_req_id != stale_connect_req_id {
            return Err((None, DbError::Disconnected));
        }
        if !Arc::ptr_eq(&entry.session, stale_session) {
            return Ok((Arc::clone(&entry.session), entry.connect_req_id));
        }
        (
            Arc::clone(&entry.reconnect_gate),
            entry.profile.clone(),
            entry.password.clone(),
            entry.database.clone(),
        )
    };

    let mut gate_guard = gate.lock().await;

    {
        let map = sessions.lock().await;
        let Some(entry) = map.get(&session_id) else {
            return Err((None, DbError::Disconnected));
        };
        if entry.connect_req_id != stale_connect_req_id {
            return Err((None, DbError::Disconnected));
        }
        if !Arc::ptr_eq(&entry.session, stale_session) {
            return Ok((Arc::clone(&entry.session), entry.connect_req_id));
        }
    }

    if let Some((failed_at, ref err)) = gate_guard.last_failure
        && failed_at >= call_started
    {
        let still_current = current_connect_req_id(sessions, session_id, stale_connect_req_id).await;
        return Err((still_current, err.clone()));
    }

    let pw = if cached_pw.is_some() {
        cached_pw
    } else if profile.password_mode == PasswordMode::Keychain {
        let rx = credentials.enqueue_read(&profile.credential_service());
        match resolve_password_with_rx(&profile, None, Some(PendingCredentialRx::Read(rx))).await {
            Ok(pw) => pw,
            Err(msg) => {
                let err = DbError::ConnectionFailed(format!("keychain read failed: {msg}"));
                gate_guard.last_failure = Some((Instant::now(), err.clone()));
                let still_current =
                    current_connect_req_id(sessions, session_id, stale_connect_req_id).await;
                return Err((still_current, err));
            }
        }
    } else {
        None
    };

    match (connector)(profile, pw.clone(), database).await {
        Ok((new_session, _connect_logs)) => {
            gate_guard.last_failure = None;
            let mut map = sessions.lock().await;
            let Some(entry) = map.get_mut(&session_id) else {
                return Err((None, DbError::Disconnected));
            };
            if entry.connect_req_id != stale_connect_req_id {
                return Err((None, DbError::Disconnected));
            }
            entry.session = Arc::clone(&new_session);
            if entry.password.is_none() {
                entry.password = pw;
            }
            Ok((new_session, entry.connect_req_id))
        }
        Err(err) => {
            gate_guard.last_failure = Some((Instant::now(), err.clone()));
            let still_current = current_connect_req_id(sessions, session_id, stale_connect_req_id).await;
            Err((still_current, err))
        }
    }
}

async fn prepare_session(
    sessions: &SessionMap,
    connector: &SessionConnector,
    credentials: &CredentialRunner,
    session_id: SessionId,
    liveness: LivenessCheck,
    expected_connect_req_id: Option<RequestId>,
) -> Result<(Arc<dyn DbSession>, RequestId), SessionCallError> {
    let (session, connect_req_id) = {
        let map = sessions.lock().await;
        let Some(entry) = map.get(&session_id) else {
            return Err(SessionCallError::Disconnected {
                connect_req_id: None,
                cause: DbError::Disconnected,
            });
        };
        if expected_connect_req_id.is_some_and(|exp| entry.connect_req_id != exp) {
            return Err(SessionCallError::Disconnected {
                connect_req_id: None,
                cause: DbError::Disconnected,
            });
        }
        (Arc::clone(&entry.session), entry.connect_req_id)
    };

    if !supports_background_reconnect(session.engine()) {
        return Ok((session, connect_req_id));
    }

    if session.is_disconnected() {
        return reconnect_session(
            sessions,
            connector,
            credentials,
            session_id,
            &session,
            connect_req_id,
        )
        .await
        .map_err(|(cid, cause)| SessionCallError::Disconnected {
            connect_req_id: cid,
            cause,
        });
    }

    if liveness == LivenessCheck::AlwaysPing
        && let Err(err) = session.ping().await
    {
        if is_session_disconnect_err(session.as_ref(), &err) {
            return reconnect_session(
                sessions,
                connector,
                credentials,
                session_id,
                &session,
                connect_req_id,
            )
            .await
            .map_err(|(cid, cause)| SessionCallError::Disconnected {
                connect_req_id: cid,
                cause,
            });
        }
        return Err(SessionCallError::Operation {
            engine: session.engine(),
            error: err,
        });
    }

    Ok((session, connect_req_id))
}

#[derive(Clone, Copy)]
struct WorkerCtx<'a> {
    sessions: &'a SessionMap,
    connector: &'a SessionConnector,
    credentials: &'a CredentialRunner,
    event_tx: &'a mpsc::Sender<WorkerEvent>,
    waker: &'a Arc<std::sync::Mutex<Option<Waker>>>,
}

async fn run_with_reconnect<T, F, Fut>(
    ctx: WorkerCtx<'_>,
    session_id: SessionId,
    liveness: LivenessCheck,
    allow_retry_on_disconnect: bool,
    expected_connect_req_id: Option<RequestId>,
    mut op: F,
) -> Result<T, SessionCallError>
where
    F: FnMut(Arc<dyn DbSession>) -> Fut,
    Fut: std::future::Future<Output = Result<T, DbError>>,
{
    let WorkerCtx {
        sessions,
        connector,
        credentials,
        event_tx,
        waker,
    } = ctx;
    let (session, connect_req_id) = match prepare_session(
        sessions,
        connector,
        credentials,
        session_id,
        liveness,
        expected_connect_req_id,
    )
    .await
    {
        Ok(ok) => ok,
        Err(err @ SessionCallError::Operation { .. }) => {
            emit_session_mode(sessions, event_tx, waker, session_id, expected_connect_req_id).await;
            return Err(err);
        }
        Err(err) => return Err(err),
    };
    let engine = session.engine();
    match op(Arc::clone(&session)).await {
        Ok(val) => {
            emit_session_mode(sessions, event_tx, waker, session_id, Some(connect_req_id)).await;
            Ok(val)
        }
        Err(error) if is_session_disconnect_err(session.as_ref(), &error) => {
            let retry_allowed = supports_background_reconnect(engine)
                && (allow_retry_on_disconnect || matches!(error, DbError::DisconnectedBeforeDispatch(_)));
            if !retry_allowed {
                let cid = current_connect_req_id(sessions, session_id, connect_req_id).await;
                return Err(SessionCallError::Disconnected {
                    connect_req_id: cid,
                    cause: unwrap_disconnect_cause(error),
                });
            }
            let (fresh_session, fresh_cid) = reconnect_session(
                sessions,
                connector,
                credentials,
                session_id,
                &session,
                connect_req_id,
            )
            .await
                    .map_err(|(cid, cause)| SessionCallError::Disconnected {
                        connect_req_id: cid,
                        cause,
                    })?;
            let fresh_engine = fresh_session.engine();
            match op(Arc::clone(&fresh_session)).await {
                Ok(val) => {
                    emit_session_mode(sessions, event_tx, waker, session_id, Some(fresh_cid)).await;
                    Ok(val)
                }
                Err(error) if is_session_disconnect_err(fresh_session.as_ref(), &error) => {
                    let cid = current_connect_req_id(sessions, session_id, fresh_cid).await;
                    Err(SessionCallError::Disconnected {
                        connect_req_id: cid,
                        cause: unwrap_disconnect_cause(error),
                    })
                }
                Err(error) => {
                    emit_session_mode(sessions, event_tx, waker, session_id, Some(fresh_cid)).await;
                    Err(SessionCallError::Operation {
                        engine: fresh_engine,
                        error,
                    })
                }
            }
        }
        Err(error) => {
            emit_session_mode(sessions, event_tx, waker, session_id, Some(connect_req_id)).await;
            Err(SessionCallError::Operation { engine, error })
        }
    }
}

async fn worker_loop(
    mut cmd_rx: tokio::sync::mpsc::UnboundedReceiver<WorkerCmd>,
    event_tx: mpsc::Sender<WorkerEvent>,
    waker: Arc<std::sync::Mutex<Option<Waker>>>,
    connector: SessionConnector,
    credentials: CredentialRunner,
    in_memory_history: bool,
) {
    let sessions: SessionMap = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let active_cancels: QueryCancelMap = Arc::new(std::sync::Mutex::new(HashMap::new()));
    let session_epochs: SessionEpochMap = Arc::new(std::sync::Mutex::new(HashMap::new()));
    let mut next_lifecycle_epoch: u64 = 0;
    let query_store: Option<Arc<QueryStore>> = if in_memory_history {
        QueryStore::open_in_memory().ok().map(Arc::new)
    } else {
        QueryStore::open_default().ok().map(Arc::new)
    };

    while let Some(cmd) = cmd_rx.recv().await {
        let sessions = Arc::clone(&sessions);
        let active_cancels = Arc::clone(&active_cancels);
        let session_epochs = Arc::clone(&session_epochs);
        let query_store = query_store.clone();
        let event_tx = event_tx.clone();
        let waker = Arc::clone(&waker);
        let connector = Arc::clone(&connector);
        let credentials_clone = credentials.clone();

        let cred_rx = match &cmd {
            WorkerCmd::SaveCredential {
                service,
                account,
                secret,
                ..
            } => Some(PendingCredentialRx::Mutate(if secret.is_empty() {
                credentials.enqueue_delete(service)
            } else {
                credentials.enqueue_write(service, account, secret)
            })),
            WorkerCmd::DeleteCredential { service, .. } => {
                Some(PendingCredentialRx::Mutate(credentials.enqueue_delete(service)))
            }
            WorkerCmd::LoadCredentialForEdit { service, .. } => {
                Some(PendingCredentialRx::Read(credentials.enqueue_read(service)))
            }
            WorkerCmd::Connect {
                profile,
                prompt_password,
                ..
            }
            | WorkerCmd::SelectDatabase {
                profile,
                prompt_password,
                ..
            } => {
                if prompt_password.as_deref().is_some_and(|s| !s.is_empty())
                    || profile.password_mode != PasswordMode::Keychain
                {
                    None
                } else {
                    Some(PendingCredentialRx::Read(
                        credentials.enqueue_read(&profile.credential_service()),
                    ))
                }
            }
            WorkerCmd::TestConnection {
                profile,
                password_override,
                ..
            } => {
                if password_override.as_deref().is_some_and(|s| !s.is_empty())
                    || profile.password_mode != PasswordMode::Keychain
                {
                    None
                } else {
                    Some(PendingCredentialRx::Read(
                        credentials.enqueue_read(&profile.credential_service()),
                    ))
                }
            }
            _ => None,
        };

        let lifecycle_epoch = match &cmd {
            WorkerCmd::Connect { session_id, .. } | WorkerCmd::SelectDatabase { session_id, .. } => {
                next_lifecycle_epoch += 1;
                if let Ok(mut guard) = session_epochs.lock() {
                    guard.insert(*session_id, next_lifecycle_epoch);
                }
                Some(next_lifecycle_epoch)
            }
            WorkerCmd::Disconnect { session_id } => {
                next_lifecycle_epoch += 1;
                if let Ok(mut guard) = session_epochs.lock() {
                    guard.insert(*session_id, next_lifecycle_epoch);
                }
                Some(next_lifecycle_epoch)
            }
            _ => None,
        };

        let (cancel_handle, cancel_target_session) = match &cmd {
            WorkerCmd::ExecuteSql { req_id, .. } => {
                let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
                let cancel_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
                if let Ok(mut guard) = active_cancels.lock() {
                    guard.insert(
                        *req_id,
                        QueryCancelEntry {
                            tx: cancel_tx,
                            flag: Arc::clone(&cancel_flag),
                            session: None,
                        },
                    );
                }
                (Some((cancel_rx, cancel_flag)), None)
            }
            WorkerCmd::CancelQuery { req_id, .. } => {
                let target = if let Ok(guard) = active_cancels.lock()
                    && let Some(entry) = guard.get(req_id)
                {
                    entry.flag.store(true, std::sync::atomic::Ordering::Release);
                    entry.tx.send_replace(true);
                    entry.session.clone()
                } else {
                    None
                };
                (None, target)
            }
            _ => (None, None),
        };

        tokio::spawn(async move {
            handle_cmd(
                cmd,
                sessions,
                session_epochs,
                lifecycle_epoch,
                active_cancels,
                cancel_handle,
                cancel_target_session,
                query_store,
                event_tx,
                waker,
                connector,
                credentials_clone,
                cred_rx,
            )
            .await;
        });
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_cmd(
    cmd: WorkerCmd,
    sessions: SessionMap,
    session_epochs: SessionEpochMap,
    lifecycle_epoch: Option<u64>,
    active_cancels: QueryCancelMap,
    cancel_handle: Option<QueryCancelHandle>,
    cancel_target_session: Option<Arc<dyn DbSession>>,
    query_store: Option<Arc<QueryStore>>,
    event_tx: mpsc::Sender<WorkerEvent>,
    waker: Arc<std::sync::Mutex<Option<Waker>>>,
    connector: SessionConnector,
    credentials: CredentialRunner,
    cred_rx: Option<PendingCredentialRx>,
) {
    let worker_ctx = WorkerCtx {
        sessions: &sessions,
        connector: &connector,
        credentials: &credentials,
        event_tx: &event_tx,
        waker: &waker,
    };
    match cmd {
        WorkerCmd::Connect {
            session_id,
            req_id,
            profile,
            prompt_password,
            database,
            preferred_schema,
        } => {
            let explicit_db = database
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .or_else(|| profile.database.as_deref().map(str::trim).filter(|s| !s.is_empty()))
                .map(str::to_owned);

            let pw = match resolve_password_with_rx(&profile, prompt_password, cred_rx).await {
                Ok(pw) => pw,
                Err(msg) => {
                    if is_current_lifecycle_epoch(&session_epochs, session_id, lifecycle_epoch) {
                        emit(
                            &event_tx,
                            &waker,
                            WorkerEvent::ConnectFailed {
                                session_id,
                                req_id,
                                error: DbError::ConnectionFailed(format!("keychain read failed: {msg}")),
                            },
                        );
                    }
                    return;
                }
            };
            let connected = (connector)(profile.clone(), pw.clone(), explicit_db.clone()).await;
            let (session, mut logs) = match connected {
                Ok(ok) => ok,
                Err(error) => {
                    if is_current_lifecycle_epoch(&session_epochs, session_id, lifecycle_epoch) {
                        emit(&event_tx, &waker, WorkerEvent::ConnectFailed { session_id, req_id, error });
                    }
                    return;
                }
            };
            let server_info = session.server_info().clone();
            let active_database = if !profile.engine.supports_multi_database() {
                Some(server_info.current_database.clone())
            } else {
                explicit_db.clone()
            };
            let reconnect_db = explicit_db.or_else(|| {
                let cur = server_info.current_database.trim();
                (!cur.is_empty()).then(|| cur.to_owned())
            });

            let (databases, db_log) = match session.list_databases().await {
                Ok(res) => res,
                Err(error) => {
                    if is_current_lifecycle_epoch(&session_epochs, session_id, lifecycle_epoch) {
                        emit(&event_tx, &waker, WorkerEvent::ConnectFailed { session_id, req_id, error });
                    }
                    return;
                }
            };
            logs.push(db_log);

            let (schemas, active_schema, objects) = if active_database.is_some() {
                match session.list_schemas().await {
                    Ok((schemas, schema_log)) => {
                        logs.push(schema_log);
                        let chosen = pick_default_schema(&schemas, preferred_schema.as_deref());
                        let objs = if let Some(ref schema_name) = chosen {
                            match session.list_objects(schema_name).await {
                                Ok((objs, obj_log)) => {
                                    logs.push(obj_log);
                                    objs
                                }
                                Err(error) => {
                                    if is_current_lifecycle_epoch(&session_epochs, session_id, lifecycle_epoch) {
                                        emit(
                                            &event_tx,
                                            &waker,
                                            WorkerEvent::ConnectFailed { session_id, req_id, error },
                                        );
                                    }
                                    return;
                                }
                            }
                        } else {
                            Vec::new()
                        };
                        (schemas, chosen, objs)
                    }
                    Err(error) => {
                        if is_current_lifecycle_epoch(&session_epochs, session_id, lifecycle_epoch) {
                            emit(&event_tx, &waker, WorkerEvent::ConnectFailed { session_id, req_id, error });
                        }
                        return;
                    }
                }
            } else {
                (Vec::new(), None, Vec::new())
            };

            let mysql_quote_mode = session.mysql_quote_mode();
            let postgres_session_mode = session.postgres_session_mode();
            {
                let mut map = sessions.lock().await;
                if !is_current_lifecycle_epoch(&session_epochs, session_id, lifecycle_epoch) {
                    return;
                }
                map.insert(
                    session_id,
                    SessionEntry {
                        connect_req_id: req_id,
                        mode_seq: 0,
                        session,
                        profile,
                        password: pw,
                        database: reconnect_db,
                        reconnect_gate: Arc::new(tokio::sync::Mutex::new(ReconnectGate::default())),
                    },
                );
                emit(
                    &event_tx,
                    &waker,
                    WorkerEvent::Connected {
                        session_id,
                        req_id,
                        server_info,
                        mysql_quote_mode,
                        postgres_session_mode,
                        databases,
                        active_database,
                        schemas,
                        active_schema,
                        objects,
                        logs,
                    },
                );
            }
        }

        WorkerCmd::UpdateSessionProfile {
            session_id,
            profile,
            password_update,
        } => {
            let mut map = sessions.lock().await;
            if let Some(entry) = map.get_mut(&session_id) {
                entry.profile = profile;
                if let Some(updated_pw) = password_update {
                    entry.password = updated_pw;
                }
            }
        }

        WorkerCmd::SelectDatabase {
            session_id,
            req_id,
            profile,
            prompt_password,
            database,
        } => {
            let pw = match resolve_password_with_rx(&profile, prompt_password, cred_rx).await {
                Ok(pw) => pw,
                Err(msg) => {
                    if is_current_lifecycle_epoch(&session_epochs, session_id, lifecycle_epoch) {
                        emit(
                            &event_tx,
                            &waker,
                            WorkerEvent::OperationFailed {
                                session_id,
                                req_id,
                                error: DbError::ConnectionFailed(format!("keychain read failed: {msg}")),
                            },
                        );
                    }
                    return;
                }
            };
            let connected = (connector)(profile.clone(), pw.clone(), Some(database.clone())).await;
            let (session, mut logs) = match connected {
                Ok(ok) => ok,
                Err(error) => {
                    if is_current_lifecycle_epoch(&session_epochs, session_id, lifecycle_epoch) {
                        emit(&event_tx, &waker, WorkerEvent::OperationFailed { session_id, req_id, error });
                    }
                    return;
                }
            };
            let databases = match session.list_databases().await {
                Ok((dbs, log)) => {
                    logs.push(log);
                    dbs
                }
                Err(error) => {
                    if is_current_lifecycle_epoch(&session_epochs, session_id, lifecycle_epoch) {
                        emit(&event_tx, &waker, WorkerEvent::OperationFailed { session_id, req_id, error });
                    }
                    return;
                }
            };
            let (schemas, schema_log) = match session.list_schemas().await {
                Ok(res) => res,
                Err(error) => {
                    if is_current_lifecycle_epoch(&session_epochs, session_id, lifecycle_epoch) {
                        emit(&event_tx, &waker, WorkerEvent::OperationFailed { session_id, req_id, error });
                    }
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
                        if is_current_lifecycle_epoch(&session_epochs, session_id, lifecycle_epoch) {
                            emit(&event_tx, &waker, WorkerEvent::OperationFailed { session_id, req_id, error });
                        }
                        return;
                    }
                }
            } else {
                Vec::new()
            };

            let server_version = session.server_info().version.clone();
            let mysql_quote_mode = session.mysql_quote_mode();
            let postgres_session_mode = session.postgres_session_mode();
            {
                let mut map = sessions.lock().await;
                if !is_current_lifecycle_epoch(&session_epochs, session_id, lifecycle_epoch) {
                    return;
                }
                map.insert(
                    session_id,
                    SessionEntry {
                        connect_req_id: req_id,
                        mode_seq: 0,
                        session,
                        profile,
                        password: pw,
                        database: Some(database.clone()),
                        reconnect_gate: Arc::new(tokio::sync::Mutex::new(ReconnectGate::default())),
                    },
                );
                emit(
                    &event_tx,
                    &waker,
                    WorkerEvent::SchemasAndObjectsLoaded {
                        session_id,
                        req_id,
                        is_db_switch: true,
                        server_version: Some(server_version),
                        mysql_quote_mode,
                        postgres_session_mode,
                        databases,
                        active_database: database,
                        schemas,
                        active_schema,
                        objects,
                        logs,
                    },
                );
            }
        }

        WorkerCmd::SelectSchema {
            session_id,
            req_id,
            connect_req_id,
            schema,
        } => {
            let outcome = run_with_reconnect(
                worker_ctx,
                session_id,
                LivenessCheck::Fast,
                true,
                Some(connect_req_id),
                |session| {
                    let schema = schema.clone();
                    async move { session.list_objects(&schema).await }
                },
            )
            .await;

            match outcome {
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
                Err(SessionCallError::Disconnected { connect_req_id, cause }) => {
                    emit_session_disconnected(&event_tx, &waker, session_id, connect_req_id, cause);
                    emit(
                        &event_tx,
                        &waker,
                        WorkerEvent::OperationFailed {
                            session_id,
                            req_id,
                            error: DbError::Disconnected,
                        },
                    );
                }
                Err(SessionCallError::Operation { error, .. }) => emit(
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
            let outcome = run_with_reconnect(
                worker_ctx,
                session_id,
                LivenessCheck::Fast,
                true,
                None,
                |session| {
                    let schema = schema.clone();
                    let object = object.clone();
                    async move { session.list_columns(&schema, &object).await }
                },
            )
            .await;

            match outcome {
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
                Err(SessionCallError::Disconnected { connect_req_id, cause }) => {
                    emit_session_disconnected(&event_tx, &waker, session_id, connect_req_id, cause);
                    emit(
                        &event_tx,
                        &waker,
                        WorkerEvent::ColumnsLoaded {
                            session_id,
                            req_id,
                            schema,
                            object,
                            columns: Err(DbError::Disconnected),
                            logs: Vec::new(),
                        },
                    );
                }
                Err(SessionCallError::Operation { error, .. }) => emit(
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
            connect_req_id,
            active_schema,
        } => {
            let outcome = run_with_reconnect(
                worker_ctx,
                session_id,
                LivenessCheck::Fast,
                true,
                Some(connect_req_id),
                |session| {
                    let active_schema = active_schema.clone();
                    async move {
                        let mut logs = Vec::new();
                        let (databases, db_log) = session.list_databases().await?;
                        logs.push(db_log);
                        let (schemas, schema_log) = session.list_schemas().await?;
                        logs.push(schema_log);
                        let chosen = pick_default_schema(&schemas, active_schema.as_deref());
                        let objects = if let Some(ref schema_name) = chosen {
                            let (objs, obj_log) = session.list_objects(schema_name).await?;
                            logs.push(obj_log);
                            objs
                        } else {
                            Vec::new()
                        };
                        let active_database = session.server_info().current_database.clone();
                        Ok((
                            databases,
                            active_database,
                            schemas,
                            chosen,
                            objects,
                            logs,
                        ))
                    }
                },
            )
            .await;

            match outcome {
                Ok((
                    databases,
                    active_database,
                    schemas,
                    chosen,
                    objects,
                    logs,
                )) => emit(
                    &event_tx,
                    &waker,
                    WorkerEvent::SchemasAndObjectsLoaded {
                        session_id,
                        req_id,
                        is_db_switch: false,
                        server_version: None,
                        mysql_quote_mode: None,
                        postgres_session_mode: None,
                        databases,
                        active_database,
                        schemas,
                        active_schema: chosen,
                        objects,
                        logs,
                    },
                ),
                Err(SessionCallError::Disconnected { connect_req_id, cause }) => {
                    emit_session_disconnected(&event_tx, &waker, session_id, connect_req_id, cause);
                    emit(
                        &event_tx,
                        &waker,
                        WorkerEvent::OperationFailed {
                            session_id,
                            req_id,
                            error: DbError::Disconnected,
                        },
                    );
                }
                Err(SessionCallError::Operation { error, .. }) => emit(
                    &event_tx,
                    &waker,
                    WorkerEvent::OperationFailed { session_id, req_id, error },
                ),
            }
        }

        WorkerCmd::FetchTablePage {
            session_id,
            tab_id,
            req_id,
            request,
        } => {
            let outcome = run_with_reconnect(
                worker_ctx,
                session_id,
                LivenessCheck::Fast,
                true,
                None,
                |session| {
                    let req = request.clone();
                    async move { session.fetch_table_page(&req).await }
                },
            )
            .await;

            match outcome {
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
                Err(SessionCallError::Disconnected { connect_req_id, cause }) => {
                    emit_session_disconnected(&event_tx, &waker, session_id, connect_req_id, cause);
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
                }
                Err(SessionCallError::Operation { engine, error }) => {
                    let failed_sql = format_failed_page_sql(engine, &request);
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
            let outcome = run_with_reconnect(
                worker_ctx,
                session_id,
                LivenessCheck::Fast,
                true,
                None,
                |session| {
                    let schema = schema.clone();
                    let table = table.clone();
                    async move { session.describe_table(&schema, &table).await }
                },
            )
            .await;

            match outcome {
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
                Err(SessionCallError::Disconnected { connect_req_id, cause }) => {
                    emit_session_disconnected(&event_tx, &waker, session_id, connect_req_id, cause);
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
                }
                Err(SessionCallError::Operation { error, .. }) => emit(
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
            engine,
            connect_req_id,
            connection_id,
            connection_name,
            database,
            statements,
            executed_sql_text,
            row_limit,
            record_history,
            expected_sql_mode,
        } => {
            let Some((mut cancel_rx, cancel_flag)) = cancel_handle else {
                return;
            };
            let _cancel_guard = CancelRegistration {
                map: Arc::clone(&active_cancels),
                req_id,
            };

            let start = Instant::now();
            let mut ticker = tokio::time::interval(Duration::from_millis(100));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            ticker.tick().await;

            let initial_sql_mode = SqlSessionMode {
                mysql: if engine == DbEngine::Mysql {
                    expected_sql_mode.mysql
                } else {
                    None
                },
                postgres: if engine == DbEngine::Postgres {
                    expected_sql_mode.postgres.clone()
                } else {
                    None
                },
            };
            let mut skipped_for_mode_mismatch = false;
            let outcome = if cancel_flag.load(std::sync::atomic::Ordering::Acquire) || *cancel_rx.borrow() {
                Err(SessionCallError::Operation {
                    engine: DbEngine::Postgres,
                    error: DbError::Cancelled,
                })
            } else {
                let prep_fut = prepare_session(
                    &sessions,
                    &connector,
                    &credentials,
                    session_id,
                    LivenessCheck::AlwaysPing,
                    Some(connect_req_id),
                );
                tokio::pin!(prep_fut);

                let prepared = loop {
                    tokio::select! {
                        res = &mut prep_fut => break Some(res),
                        Ok(()) = cancel_rx.changed() => {
                            if *cancel_rx.borrow() {
                                break None;
                            }
                        }
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

                match prepared {
                    None => Err(SessionCallError::Operation {
                        engine: DbEngine::Postgres,
                        error: DbError::Cancelled,
                    }),
                    Some(Err(err)) => Err(err),
                    Some(Ok((mut session, mut session_cid))) => {
                        let cancel_key = req_id.get();
                        let mut retried_pre_dispatch = false;
                        loop {
                            let pre_cancelled = if let Ok(mut guard) = active_cancels.lock() {
                                match guard.get_mut(&req_id) {
                                    Some(entry) if !entry.flag.load(std::sync::atomic::Ordering::Acquire) => {
                                        entry.session = Some(Arc::clone(&session));
                                        false
                                    }
                                    _ => true,
                                }
                            } else {
                                cancel_flag.load(std::sync::atomic::Ordering::Acquire) || *cancel_rx.borrow()
                            };
                            if pre_cancelled || *cancel_rx.borrow() {
                                break Err(SessionCallError::Operation {
                                    engine: session.engine(),
                                    error: DbError::Cancelled,
                                });
                            }
                            let exec_res = {
                                let exec_fut = session.execute_sql_cancellable_checked(
                                    &statements,
                                    row_limit,
                                    cancel_key,
                                    Some(cancel_flag.as_ref()),
                                    expected_sql_mode.clone(),
                                );
                                tokio::pin!(exec_fut);
                                loop {
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
                                }
                            };
                            match exec_res {
                                Ok((batch, logs)) => {
                                    break Ok((batch, logs, session_cid));
                                }
                                Err(err @ (DbError::MysqlModeMismatch | DbError::PostgresStringModeMismatch)) => {
                                    skipped_for_mode_mismatch = true;
                                    break Err(SessionCallError::Operation {
                                        engine: session.engine(),
                                        error: err,
                                    });
                                }
                                Err(DbError::DisconnectedBeforeDispatch(cause)) => {
                                    if cancel_flag.load(std::sync::atomic::Ordering::Acquire) || *cancel_rx.borrow() {
                                        break Err(SessionCallError::Operation {
                                            engine: session.engine(),
                                            error: DbError::Cancelled,
                                        });
                                    }
                                    if retried_pre_dispatch || !supports_background_reconnect(session.engine()) {
                                        let cid = current_connect_req_id(&sessions, session_id, session_cid).await;
                                        break Err(SessionCallError::Disconnected {
                                            connect_req_id: cid,
                                            cause: *cause,
                                        });
                                    }
                                    retried_pre_dispatch = true;
                                    if let Ok(mut guard) = active_cancels.lock()
                                        && let Some(entry) = guard.get_mut(&req_id)
                                    {
                                        entry.session = None;
                                    }
                                    let reconnected = {
                                        let rec_fut = reconnect_session(
                                            &sessions,
                                            &connector,
                                            &credentials,
                                            session_id,
                                            &session,
                                            session_cid,
                                        );
                                        tokio::pin!(rec_fut);
                                        loop {
                                            tokio::select! {
                                                res = &mut rec_fut => break Some(res),
                                                Ok(()) = cancel_rx.changed() => {
                                                    if *cancel_rx.borrow() {
                                                        break None;
                                                    }
                                                }
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
                                        }
                                    };
                                    match reconnected {
                                        None => {
                                            break Err(SessionCallError::Operation {
                                                engine: session.engine(),
                                                error: DbError::Cancelled,
                                            });
                                        }
                                        Some(Err((cid, cause))) => {
                                            break Err(SessionCallError::Disconnected {
                                                connect_req_id: cid,
                                                cause,
                                            });
                                        }
                                        Some(Ok((fresh_session, fresh_cid))) => {
                                            session = fresh_session;
                                            session_cid = fresh_cid;
                                        }
                                    }
                                }
                                Err(error) if is_session_disconnect_err(session.as_ref(), &error) => {
                                    let cid = current_connect_req_id(&sessions, session_id, session_cid).await;
                                    break Err(SessionCallError::Disconnected {
                                        connect_req_id: cid,
                                        cause: unwrap_disconnect_cause(error),
                                    });
                                }
                                Err(error) => {
                                    break Err(SessionCallError::Operation {
                                        engine: session.engine(),
                                        error,
                                    });
                                }
                            }
                        }
                    }
                }
            };

            let (result, logs) = match outcome {
                Ok((batch, logs, cid)) => {
                    emit_session_mode(&sessions, &event_tx, &waker, session_id, Some(cid)).await;
                    (Ok(batch), logs)
                }
                Err(SessionCallError::Disconnected { connect_req_id: cid, cause }) => {
                    emit_session_disconnected(&event_tx, &waker, session_id, cid, cause);
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
                }
                Err(SessionCallError::Operation { error, .. }) => {
                    emit_session_mode(
                        &sessions,
                        &event_tx,
                        &waker,
                        session_id,
                        Some(connect_req_id),
                    )
                    .await;
                    (Err(error), Vec::new())
                }
            };

            let has_credentials = batch_sets_credentials_with_session_mode(
                engine,
                &statements,
                &executed_sql_text,
                initial_sql_mode,
            );
            let recorded_history = if !skipped_for_mode_mismatch
                && record_history
                && !has_credentials
                && let Some(store) = query_store
                && !trim_sql_whitespace_for_engine(engine, &executed_sql_text).is_empty()
            {
                let (duration_ms, rows_count, error) = match &result {
                    Ok(batch) => {
                        let err = if batch.cancelled {
                            Some("Query cancelled".to_owned())
                        } else if batch.results.iter().any(|r| r.error.is_some()) {
                            // Omit raw backend error strings from local history so constraints or data values are not leaked.
                            Some("Query failed".to_owned())
                        } else {
                            None
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
                    Err(DbError::Cancelled) => {
                        (start.elapsed().as_millis() as u64, None, Some("Query cancelled".to_owned()))
                    }
                    Err(_) => (start.elapsed().as_millis() as u64, None, Some("Query failed".to_owned())),
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

        WorkerCmd::CancelQuery { req_id, .. } => {
            if let Some(session) = cancel_target_session {
                drop(session.cancel_query_by_key(req_id.get()).await);
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
            let mut map = sessions.lock().await;
            if is_current_lifecycle_epoch(&session_epochs, session_id, lifecycle_epoch) {
                map.remove(&session_id);
            }
        }

        WorkerCmd::TestConnection {
            req_id,
            profile,
            password_override,
        } => {
            let result = match resolve_password_with_rx(&profile, password_override, cred_rx).await {
                Ok(pw) => test_connection(&profile, pw.as_deref()).await,
                Err(msg) => Err(DbError::ConnectionFailed(format!("keychain read failed: {msg}"))),
            };
            emit(&event_tx, &waker, WorkerEvent::TestResult { req_id, result });
        }

        WorkerCmd::SaveCredential {
            req_id,
            connection_id,
            ..
        } => {
            let result = match cred_rx {
                Some(PendingCredentialRx::Mutate(rx)) => rx
                    .await
                    .map_err(|_| "credential worker channel closed".to_owned())
                    .and_then(|res| res),
                _ => Ok(()),
            };
            emit(
                &event_tx,
                &waker,
                WorkerEvent::CredentialSaved {
                    req_id,
                    connection_id,
                    result,
                },
            );
        }

        WorkerCmd::DeleteCredential {
            req_id,
            connection_id,
            ..
        } => {
            let result = match cred_rx {
                Some(PendingCredentialRx::Mutate(rx)) => rx
                    .await
                    .map_err(|_| "credential worker channel closed".to_owned())
                    .and_then(|res| res),
                _ => Ok(()),
            };
            emit(
                &event_tx,
                &waker,
                WorkerEvent::CredentialDeleted {
                    req_id,
                    connection_id,
                    result,
                },
            );
        }

        WorkerCmd::LoadCredentialForEdit {
            req_id,
            connection_id,
            ..
        } => {
            let result = match cred_rx {
                Some(PendingCredentialRx::Read(rx)) => rx
                    .await
                    .map_err(|_| "credential worker channel closed".to_owned())
                    .and_then(|res| res),
                _ => Ok(None),
            };
            emit(
                &event_tx,
                &waker,
                WorkerEvent::CredentialLoaded {
                    req_id,
                    connection_id,
                    result,
                },
            );
        }

        WorkerCmd::CommitStagedChanges {
            session_id,
            tab_id,
            req_id,
            connect_req_id,
            statements,
        } => {
            let outcome = run_with_reconnect(
                worker_ctx,
                session_id,
                LivenessCheck::AlwaysPing,
                false,
                Some(connect_req_id),
                |session| {
                    let stmts = statements.clone();
                    async move { session.execute_transaction(&stmts).await }
                },
            )
            .await;

            match outcome {
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
                Err(SessionCallError::Disconnected { connect_req_id, cause }) => {
                    emit_session_disconnected(&event_tx, &waker, session_id, connect_req_id, cause);
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
                }
                Err(SessionCallError::Operation { error, .. }) => {
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
            connect_req_id,
            statement,
            analyze,
            expected_sql_mode,
        } => {
            let liveness = if analyze {
                LivenessCheck::AlwaysPing
            } else {
                LivenessCheck::Fast
            };
            let outcome = run_with_reconnect(
                worker_ctx,
                session_id,
                liveness,
                !analyze,
                Some(connect_req_id),
                |session| {
                    let stmt = statement.clone();
                    let expected_mode = expected_sql_mode.clone();
                    async move { session.explain_sql_checked(&stmt, analyze, expected_mode).await }
                },
            )
            .await;

            match outcome {
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
                Err(SessionCallError::Disconnected { connect_req_id: cid, cause }) => {
                    emit_session_disconnected(&event_tx, &waker, session_id, cid, cause);
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
                }
                Err(SessionCallError::Operation { error, .. }) => emit(
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
            let outcome = run_with_reconnect(
                worker_ctx,
                session_id,
                LivenessCheck::Fast,
                true,
                None,
                |session| {
                    let fk = fk.clone();
                    let value = value.clone();
                    let source_col = source_col.clone();
                    async move { session.fetch_fk_row(&fk, &value, &source_col).await }
                },
            )
            .await;

            match outcome {
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
                Err(SessionCallError::Disconnected { connect_req_id, cause }) => {
                    emit_session_disconnected(&event_tx, &waker, session_id, connect_req_id, cause);
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
                }
                Err(SessionCallError::Operation { error, .. }) => emit(
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

fn format_failed_page_sql(engine: DbEngine, req: &PageRequest) -> String {
    let mut sql = format!("SELECT * FROM {}", engine.qualify_ident(&req.schema, &req.table));
    if let Some(where_clause) = &req.filter_where {
        let trimmed = where_clause.trim();
        if !trimmed.is_empty() {
            sql.push_str(&format!(" WHERE ({trimmed})"));
        }
    }
    if let Some(sort) = &req.sort {
        let dir = if sort.ascending { "ASC" } else { "DESC" };
        let col = engine.quote_ident(&sort.column);
        if engine == DbEngine::Mysql {
            sql.push_str(&format!(" ORDER BY ({col} IS NULL) ASC, {col} {dir}"));
        } else {
            sql.push_str(&format!(" ORDER BY {col} {dir} NULLS LAST"));
        }
    }
    sql.push_str(&format!(" LIMIT {} OFFSET {}", req.limit, req.offset));
    sql
}
