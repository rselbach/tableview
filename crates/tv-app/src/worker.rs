use std::collections::HashMap;
use std::sync::{Arc, Mutex, mpsc};

use rune::Waker;
use rune::credentials::Credentials;
use tv_core::engine::postgres::PostgresSession;
use tv_core::{
    ColumnSummary, ConnectionId, ConnectionProfile, DatabaseInfo, DbError, DbObject, DbSession, ExecutedSql,
    PasswordMode, RequestId, SchemaInfo, ServerInfo, SessionId,
};

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
}

/// Owns the background Tokio runtime thread and wakes Rune whenever worker events arrive.
pub struct WorkerBridge {
    cmd_tx: tokio::sync::mpsc::UnboundedSender<WorkerCmd>,
    event_rx: mpsc::Receiver<WorkerEvent>,
    waker: Arc<Mutex<Option<Waker>>>,
    _thread: Option<std::thread::JoinHandle<()>>,
}

impl WorkerBridge {
    /// Spawns the background Tokio worker thread. Returns `None` if runtime initialization fails.
    pub fn spawn() -> Option<Self> {
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::channel();
        let waker: Arc<Mutex<Option<Waker>>> = Arc::new(Mutex::new(None));
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
                rt.block_on(worker_loop(cmd_rx, event_tx, worker_waker));
            })
            .ok()?;

        Some(Self {
            cmd_tx,
            event_rx,
            waker,
            _thread: Some(thread),
        })
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

fn emit(tx: &mpsc::Sender<WorkerEvent>, waker: &Arc<Mutex<Option<Waker>>>, event: WorkerEvent) {
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
    waker: Arc<Mutex<Option<Waker>>>,
) {
    let sessions: Arc<tokio::sync::Mutex<HashMap<SessionId, Arc<PostgresSession>>>> =
        Arc::new(tokio::sync::Mutex::new(HashMap::new()));

    while let Some(cmd) = cmd_rx.recv().await {
        let sessions = Arc::clone(&sessions);
        let event_tx = event_tx.clone();
        let waker = Arc::clone(&waker);

        tokio::spawn(async move {
            handle_cmd(cmd, sessions, event_tx, waker).await;
        });
    }
}

async fn handle_cmd(
    cmd: WorkerCmd,
    sessions: Arc<tokio::sync::Mutex<HashMap<SessionId, Arc<PostgresSession>>>>,
    event_tx: mpsc::Sender<WorkerEvent>,
    waker: Arc<Mutex<Option<Waker>>>,
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
    }
}
