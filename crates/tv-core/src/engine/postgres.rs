use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context as TaskContext, Poll};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::pem::{Error as PemError, PemObject};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, TrustAnchor, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, Error as TlsError, RootCertStore, SignatureScheme};
use webpki::{EndEntityCert, KeyUsage, anchor_from_trusted_cert};
use tokio::task::JoinHandle;
use tokio_postgres::{Config, NoTls, SimpleQueryMessage};
use tokio_postgres_rustls::MakeRustlsConnect;

use crate::connection::{ConnectionProfile, DbEngine, PasswordMode, SslConfig, SslMode};
use crate::engine::{
    CellValue, ColumnMeta, ColumnSummary, DatabaseInfo, DbError, DbObject, DbObjectKind, DbSession, ExecutedSql,
    FkPeekRow, ForeignKeyRef, IndexInfo, PageRequest, QueryBatchResult, QueryStatementResult, SchemaInfo, ServerInfo,
    TablePage, TableStats, TableStructure, TransactionOutcome,
};
use crate::explain::{ExplainPlan, build_explain_sql};
use crate::staging::build_fk_where_clause;
use std::collections::HashMap;

static CRYPTO_PROVIDER_INIT: OnceLock<()> = OnceLock::new();
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const PING_TIMEOUT: Duration = Duration::from_secs(5);

fn ensure_crypto_provider() {
    CRYPTO_PROVIDER_INIT.get_or_init(|| {
        drop(rustls::crypto::ring::default_provider().install_default());
    });
}

#[derive(Default)]
struct PostgresCancelState {
    active_key: u64,
    in_flight_cancels: usize,
    cancel_requested: bool,
}

struct ActiveCancelKeyGuard<'a>(&'a Mutex<PostgresCancelState>);

impl Drop for ActiveCancelKeyGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut state) = self.0.lock() {
            state.active_key = 0;
        }
    }
}

struct InFlightCancelGuard<'a> {
    state: &'a Mutex<PostgresCancelState>,
    tx: &'a tokio::sync::watch::Sender<usize>,
}

impl Drop for InFlightCancelGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut state) = self.state.lock() {
            state.in_flight_cancels = state.in_flight_cancels.saturating_sub(1);
            self.tx.send_replace(state.in_flight_cancels);
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct PgSqlstateError<'a> {
    code: Option<&'a str>,
    is_top_level_syntax_error: bool,
    has_error_context: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PgLiveSessionState {
    reset_scs: Option<bool>,
    tx_state: Option<crate::sql::PostgresTxState>,
    session_scs: Option<bool>,
    pre_tx_scs: Option<bool>,
    savepoints: Vec<crate::sql::PostgresSavepointMode>,
    has_untracked_savepoints: bool,
    max_ident_bytes: usize,
    server_encoding: Option<crate::sql::PostgresServerEncoding>,
}

impl PgLiveSessionState {
    fn new(live_scs: bool, reset_scs: Option<bool>) -> Self {
        Self {
            reset_scs,
            tx_state: Some(crate::sql::PostgresTxState::Idle),
            session_scs: Some(live_scs),
            pre_tx_scs: Some(live_scs),
            savepoints: Vec::new(),
            has_untracked_savepoints: false,
            max_ident_bytes: crate::sql::PG_DEFAULT_MAX_IDENT_BYTES,
            server_encoding: None,
        }
    }

    fn to_session_mode(&self, live_scs: bool) -> crate::sql::PostgresSessionMode {
        crate::sql::PostgresSessionMode {
            standard_conforming_strings: live_scs,
            reset_standard_conforming_strings: self.reset_scs,
            tx_state: self.tx_state,
            session_standard_conforming_strings: self.session_scs,
            pre_tx_standard_conforming_strings: self.pre_tx_scs,
            savepoints: self.savepoints.clone(),
            has_untracked_savepoints: self.has_untracked_savepoints,
            max_identifier_length: self.max_ident_bytes,
            server_encoding: self.server_encoding,
        }
    }

    fn apply_success(&mut self, stmt_sql: &str, before_scs: bool, after_scs: bool) {
        use crate::sql::{
            MAX_TRACKED_PG_SAVEPOINTS, PgScsChange, PostgresSavepointMode, PostgresTxState,
            parse_inline_pg_scs_change,
        };

        match parse_inline_pg_scs_change(
            stmt_sql,
            before_scs,
            self.tx_state,
            self.max_ident_bytes,
            self.server_encoding,
        ) {
            None => {
                if self.tx_state == Some(PostgresTxState::Idle) {
                    self.session_scs = Some(after_scs);
                    self.pre_tx_scs = Some(after_scs);
                } else if before_scs != after_scs {
                    self.session_scs = None;
                }
            }
            Some(PgScsChange::BeginTx) => match self.tx_state {
                Some(PostgresTxState::InTransaction | PostgresTxState::Aborted) => {}
                Some(PostgresTxState::Idle) => {
                    self.tx_state = Some(PostgresTxState::InTransaction);
                    self.pre_tx_scs = Some(after_scs);
                    self.session_scs = Some(before_scs);
                    self.savepoints.clear();
                    self.has_untracked_savepoints = false;
                }
                None => {
                    self.tx_state = Some(PostgresTxState::InTransaction);
                    self.pre_tx_scs = None;
                    self.savepoints.clear();
                    self.has_untracked_savepoints = true;
                }
            },
            Some(PgScsChange::CommitTx { chain } | PgScsChange::RollbackTx { chain }) => {
                self.savepoints.clear();
                self.has_untracked_savepoints = false;
                self.tx_state = Some(if chain && self.tx_state != Some(PostgresTxState::Idle) {
                    PostgresTxState::InTransaction
                } else {
                    PostgresTxState::Idle
                });
                self.session_scs = Some(after_scs);
                self.pre_tx_scs = Some(after_scs);
            }
            Some(PgScsChange::PrepareTx) => {
                self.savepoints.clear();
                self.has_untracked_savepoints = false;
                self.tx_state = Some(PostgresTxState::Idle);
                self.session_scs = Some(after_scs);
                self.pre_tx_scs = Some(after_scs);
            }
            Some(PgScsChange::Savepoint(maybe_name)) => {
                if self.tx_state == Some(PostgresTxState::Idle) {
                    self.pre_tx_scs = None;
                    self.savepoints.clear();
                    self.has_untracked_savepoints = true;
                }
                self.tx_state = Some(PostgresTxState::InTransaction);
                if let Some(name) = maybe_name {
                    if self.savepoints.len() >= MAX_TRACKED_PG_SAVEPOINTS {
                        self.savepoints.clear();
                        self.has_untracked_savepoints = true;
                    }
                    self.savepoints.push(PostgresSavepointMode {
                        name,
                        active_standard_conforming_strings: Some(after_scs),
                        session_standard_conforming_strings: self.session_scs,
                    });
                } else {
                    self.savepoints.clear();
                    self.has_untracked_savepoints = true;
                }
            }
            Some(PgScsChange::ReleaseSavepoint(maybe_name)) => {
                self.tx_state = Some(PostgresTxState::InTransaction);
                if let Some(name) = maybe_name
                    && let Some(pos) = self.savepoints.iter().rposition(|sp| sp.name == name)
                {
                    self.savepoints.truncate(pos);
                } else {
                    self.savepoints.clear();
                    self.has_untracked_savepoints = true;
                }
            }
            Some(PgScsChange::RollbackToSavepoint(maybe_name)) => {
                self.tx_state = Some(PostgresTxState::InTransaction);
                if let Some(name) = maybe_name
                    && let Some(pos) = self.savepoints.iter().rposition(|sp| sp.name == name)
                {
                    self.session_scs = self.savepoints[pos].session_standard_conforming_strings;
                    self.savepoints.truncate(pos + 1);
                } else {
                    self.savepoints.clear();
                    self.session_scs = None;
                    self.has_untracked_savepoints = true;
                }
            }
            Some(PgScsChange::SetSession(_)) => {
                self.session_scs = Some(after_scs);
                if self.tx_state == Some(PostgresTxState::Idle) {
                    self.pre_tx_scs = Some(after_scs);
                }
            }
            Some(PgScsChange::SetLocal(_)) => {}
            Some(PgScsChange::ResetSession) => {
                self.reset_scs = Some(after_scs);
                self.session_scs = Some(after_scs);
                if self.tx_state == Some(PostgresTxState::Idle) {
                    self.pre_tx_scs = Some(after_scs);
                }
            }
            Some(PgScsChange::ResetLocal) => {}
            Some(PgScsChange::DiscardAll) => {
                self.tx_state = Some(PostgresTxState::Idle);
                self.savepoints.clear();
                self.has_untracked_savepoints = false;
                self.reset_scs = Some(after_scs);
                self.session_scs = Some(after_scs);
                self.pre_tx_scs = Some(after_scs);
            }
            Some(PgScsChange::ReloadConf) => {
                self.reset_scs = None;
            }
            Some(PgScsChange::UncertainSetting) => match self.tx_state {
                Some(PostgresTxState::Idle) => {
                    self.session_scs = Some(after_scs);
                    self.pre_tx_scs = Some(after_scs);
                }
                Some(PostgresTxState::InTransaction) => {
                    self.session_scs = None;
                }
                Some(PostgresTxState::Aborted) | None => {
                    self.tx_state = None;
                    self.session_scs = None;
                    self.pre_tx_scs = None;
                    self.savepoints.clear();
                    self.has_untracked_savepoints = true;
                }
            },
            Some(PgScsChange::Uncertain) => {
                self.tx_state = None;
                self.session_scs = None;
                self.pre_tx_scs = None;
                self.savepoints.clear();
                self.has_untracked_savepoints = true;
            }
        }
    }

    fn apply_sqlstate_error(
        &mut self,
        stmt_sql: &str,
        before_scs: bool,
        err: PgSqlstateError<'_>,
        after_scs: bool,
    ) {
        use crate::sql::{PgScsChange, PostgresTxState, parse_inline_pg_scs_change};

        match err.code {
            Some("25P01") if !err.has_error_context => {
                self.tx_state = Some(PostgresTxState::Idle);
                self.savepoints.clear();
                self.has_untracked_savepoints = false;
                self.session_scs = Some(after_scs);
                self.pre_tx_scs = Some(after_scs);
            }
            Some(sqlstate) => {
                match parse_inline_pg_scs_change(
                    stmt_sql,
                    before_scs,
                    self.tx_state,
                    self.max_ident_bytes,
                    self.server_encoding,
                ) {
                    Some(PgScsChange::CommitTx { .. })
                        if sqlstate != "42601" || !err.is_top_level_syntax_error =>
                    {
                        self.tx_state = Some(PostgresTxState::Idle);
                        self.savepoints.clear();
                        self.has_untracked_savepoints = false;
                        self.session_scs = Some(after_scs);
                        self.pre_tx_scs = Some(after_scs);
                    }
                    Some(PgScsChange::PrepareTx)
                        if sqlstate != "42601" || !err.is_top_level_syntax_error =>
                    {
                        if self.tx_state == Some(PostgresTxState::Idle) {
                            self.savepoints.clear();
                            self.has_untracked_savepoints = false;
                            self.session_scs = Some(after_scs);
                            self.pre_tx_scs = Some(after_scs);
                        } else {
                            self.tx_state = None;
                            self.session_scs = None;
                            self.pre_tx_scs = None;
                            self.savepoints.clear();
                            self.has_untracked_savepoints = true;
                        }
                    }
                    Some(
                        PgScsChange::ReleaseSavepoint(_) | PgScsChange::RollbackToSavepoint(_),
                    ) if sqlstate == "3B001" && !err.has_error_context => {
                        if self.tx_state == Some(PostgresTxState::Idle) {
                            self.pre_tx_scs = None;
                            self.session_scs = None;
                            self.savepoints.clear();
                            self.has_untracked_savepoints = true;
                        }
                        self.tx_state = Some(PostgresTxState::Aborted);
                    }
                    _ if sqlstate == "25P02" && !err.has_error_context => {
                        if self.tx_state == Some(PostgresTxState::Idle) {
                            self.pre_tx_scs = None;
                            self.session_scs = None;
                            self.savepoints.clear();
                            self.has_untracked_savepoints = true;
                        }
                        self.tx_state = Some(PostgresTxState::Aborted);
                    }
                    _ => match self.tx_state {
                        Some(PostgresTxState::Idle) => {
                            self.session_scs = Some(after_scs);
                            self.pre_tx_scs = Some(after_scs);
                        }
                        Some(PostgresTxState::InTransaction) => {
                            self.tx_state = Some(PostgresTxState::Aborted);
                        }
                        Some(PostgresTxState::Aborted) | None => {}
                    },
                }
            }
            None => {
                self.tx_state = None;
                self.session_scs = None;
                self.pre_tx_scs = None;
                self.savepoints.clear();
                self.has_untracked_savepoints = true;
            }
        }
    }

    /// Updates transaction state after a non-editor database query fails (grid page, inspect, etc.).
    fn apply_aux_query_error(&mut self, sqlstate: Option<&str>) {
        let Some(code) = sqlstate else {
            return;
        };
        if is_pg_disconnect_sqlstate(code) {
            return;
        }
        if code == "25P02" || self.tx_state == Some(crate::sql::PostgresTxState::InTransaction) {
            self.tx_state = Some(crate::sql::PostgresTxState::Aborted);
        }
    }
}

fn is_pg_top_level_syntax_error(err: &tokio_postgres::Error, from_prepare: bool) -> bool {
    from_prepare
        || err.as_db_error().is_some_and(|db| {
            db.code().code() == "42601"
                && matches!(
                    db.position(),
                    Some(tokio_postgres::error::ErrorPosition::Original(_))
                )
                && db.where_().is_none()
        })
}

/// Active PostgreSQL database session backed by `tokio-postgres`.
pub struct PostgresSession {
    client: Arc<tokio_postgres::Client>,
    server_info: ServerInfo,
    ssl: SslConfig,
    use_tls_for_cancel: bool,
    disconnected: Arc<AtomicBool>,
    standard_conforming_strings: Arc<AtomicBool>,
    live_state: Mutex<PgLiveSessionState>,
    param_sync_tx: tokio::sync::mpsc::UnboundedSender<tokio::sync::oneshot::Sender<bool>>,
    cancel_state: Mutex<PostgresCancelState>,
    cancel_in_flight_tx: tokio::sync::watch::Sender<usize>,
    conn_task: Mutex<Option<JoinHandle<()>>>,
    client_lock: tokio::sync::Mutex<()>,
}

impl Drop for PostgresSession {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.conn_task.lock()
            && let Some(handle) = guard.take()
        {
            handle.abort();
        }
    }
}

impl PostgresSession {
    /// Connects to PostgreSQL using the supplied profile, optional runtime password, and optional database override.
    pub async fn connect(
        profile: &ConnectionProfile,
        password: Option<&str>,
        database_override: Option<&str>,
    ) -> Result<(Self, Vec<ExecutedSql>), DbError> {
        profile
            .validate()
            .map_err(|e| DbError::InvalidInput(e.to_string()))?;

        let target_db = database_override
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .or_else(|| profile.database.as_deref().map(str::trim).filter(|s| !s.is_empty()));

        let effective_db = target_db.unwrap_or("postgres");
        let (client, conn_task, disconnected, standard_conforming_strings, param_sync_tx, used_tls) =
            match connect_raw(profile, password, effective_db).await {
                Ok(ok) => ok,
                Err(err) if target_db.is_none() && is_missing_database_error(&err) && profile.user.trim() != "postgres" => {
                    connect_raw(profile, password, profile.user.trim()).await?
                }
                Err(err) => return Err(err),
            };

        let (cancel_in_flight_tx, _) = tokio::sync::watch::channel(0usize);
        let initial_scs = standard_conforming_strings.load(Ordering::Acquire);
        let mut session = Self {
            client: Arc::new(client),
            server_info: ServerInfo {
                version: String::new(),
                current_database: effective_db.to_owned(),
                current_user: profile.user.trim().to_owned(),
            },
            ssl: profile.ssl.clone(),
            use_tls_for_cancel: used_tls,
            disconnected,
            standard_conforming_strings,
            live_state: Mutex::new(PgLiveSessionState::new(initial_scs, None)),
            param_sync_tx,
            cancel_state: Mutex::new(PostgresCancelState::default()),
            cancel_in_flight_tx,
            conn_task: Mutex::new(Some(conn_task)),
            client_lock: tokio::sync::Mutex::new(()),
        };

        let (info, log) = session.fetch_server_info().await?;
        session.server_info = info;
        let live_scs = session.sync_standard_conforming_strings().await;
        let (reset_scs, max_ident_bytes, server_encoding) = session.fetch_pg_session_defaults().await;
        if let Ok(mut guard) = session.live_state.lock() {
            let mut state = PgLiveSessionState::new(live_scs, reset_scs);
            state.max_ident_bytes = max_ident_bytes;
            state.server_encoding = server_encoding;
            *guard = state;
        }
        Ok((session, vec![log]))
    }

    async fn fetch_pg_session_defaults(
        &self,
    ) -> (Option<bool>, usize, Option<crate::sql::PostgresServerEncoding>) {
        let sql = "SELECT name, reset_val, setting FROM pg_catalog.pg_settings WHERE name IN ('standard_conforming_strings', 'max_identifier_length', 'server_encoding')";
        let Ok(rows) = self.client.query(sql, &[]).await else {
            return (None, crate::sql::PG_DEFAULT_MAX_IDENT_BYTES, None);
        };
        let mut reset_scs = None;
        let mut max_ident_bytes = crate::sql::PG_DEFAULT_MAX_IDENT_BYTES;
        let mut server_encoding = None;
        for row in rows {
            let name: Option<String> = row.get(0);
            let reset_val: Option<String> = row.get(1);
            let setting: Option<String> = row.get(2);
            match name.as_deref() {
                Some("standard_conforming_strings") => {
                    reset_scs = reset_val.as_deref().and_then(crate::sql::parse_pg_bool_guc);
                }
                Some("max_identifier_length") => {
                    if let Some(parsed) = setting
                        .as_deref()
                        .and_then(|s| s.trim().parse::<usize>().ok())
                        .filter(|&n| n > 0)
                    {
                        max_ident_bytes = parsed;
                    }
                }
                Some("server_encoding") => {
                    server_encoding = setting
                        .as_deref()
                        .map(crate::sql::PostgresServerEncoding::from_pg_name);
                }
                _ => {}
            }
        }
        (reset_scs, max_ident_bytes, server_encoding)
    }

    fn current_postgres_session_mode(&self, live_scs: bool) -> crate::sql::PostgresSessionMode {
        self.live_state
            .lock()
            .map_or_else(
                |_| crate::sql::PostgresSessionMode::new(live_scs),
                |guard| guard.to_session_mode(live_scs),
            )
    }

    fn record_pg_stmt_success(&self, stmt_sql: &str, before_scs: bool, after_scs: bool) {
        if let Ok(mut guard) = self.live_state.lock() {
            guard.apply_success(stmt_sql, before_scs, after_scs);
        }
    }

    fn record_pg_stmt_error(
        &self,
        stmt_sql: &str,
        before_scs: bool,
        err: &tokio_postgres::Error,
        from_prepare: bool,
        after_scs: bool,
    ) {
        let sqlstate_err = PgSqlstateError {
            code: err.as_db_error().map(|db| db.code().code()),
            is_top_level_syntax_error: is_pg_top_level_syntax_error(err, from_prepare),
            has_error_context: err.as_db_error().is_some_and(|db| db.where_().is_some()),
        };
        if let Ok(mut guard) = self.live_state.lock() {
            guard.apply_sqlstate_error(stmt_sql, before_scs, sqlstate_err, after_scs);
        }
    }

    fn record_pg_aux_query_error(&self, err: &tokio_postgres::Error) {
        let sqlstate = err.as_db_error().map(|db| db.code().code());
        if let Ok(mut guard) = self.live_state.lock() {
            guard.apply_aux_query_error(sqlstate);
        }
    }

    fn map_aux_query_err(&self, err: tokio_postgres::Error) -> DbError {
        self.record_pg_aux_query_error(&err);
        self.map_session_err(err)
    }

    fn record_pg_transaction_ended(&self, after_scs: bool) {
        if let Ok(mut guard) = self.live_state.lock() {
            guard.tx_state = Some(crate::sql::PostgresTxState::Idle);
            guard.savepoints.clear();
            guard.has_untracked_savepoints = false;
            guard.session_scs = Some(after_scs);
            guard.pre_tx_scs = Some(after_scs);
        }
    }

    async fn sync_standard_conforming_strings(&self) -> bool {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if self.param_sync_tx.send(tx).is_err() {
            return self.standard_conforming_strings.load(Ordering::Acquire);
        }
        match rx.await {
            Ok(val) => val,
            Err(_) => self.standard_conforming_strings.load(Ordering::Acquire),
        }
    }

    /// Tests connectivity and returns concise server metadata without retaining an open session.
    pub async fn test_connection(
        profile: &ConnectionProfile,
        password: Option<&str>,
    ) -> Result<ServerInfo, DbError> {
        let (session, _) = Self::connect(profile, password, None).await?;
        Ok(session.server_info().clone())
    }

    async fn fetch_server_info(&self) -> Result<(ServerInfo, ExecutedSql), DbError> {
        let sql = "SELECT version(), current_database(), current_user";
        let start = Instant::now();
        let row = self
            .client
            .query_one(sql, &[])
            .await
            .map_err(map_pg_err)?;
        let raw_version: String = row.get(0);
        let current_database: String = row.get(1);
        let current_user: String = row.get(2);
        let log = ExecutedSql {
            sql: sql.to_owned(),
            duration_ms: start.elapsed().as_millis() as u64,
            error: None,
        };
        Ok((
            ServerInfo {
                version: concise_pg_version(&raw_version),
                current_database,
                current_user,
            },
            log,
        ))
    }

    fn ensure_alive(&self) -> Result<(), DbError> {
        if self.is_disconnected() {
            return Err(DbError::Disconnected);
        }
        Ok(())
    }

    fn map_session_err(&self, err: tokio_postgres::Error) -> DbError {
        if err.is_closed() {
            self.disconnected.store(true, Ordering::Release);
            return DbError::Disconnected;
        }
        if let Some(db) = err.as_db_error()
            && is_pg_disconnect_sqlstate(db.code().code())
        {
            self.disconnected.store(true, Ordering::Release);
            return DbError::Disconnected;
        }
        if err.as_db_error().is_none() && self.is_disconnected() {
            return DbError::Disconnected;
        }
        map_pg_err(err)
    }

    async fn fetch_column_metas(&self, schema: &str, table: &str) -> Result<(Vec<ColumnMeta>, ExecutedSql), DbError> {
        let col_sql = "SELECT \
                       a.attname, \
                       pg_catalog.format_type(a.atttypid, a.atttypmod), \
                       t.typname, \
                       NOT a.attnotnull, \
                       pg_catalog.pg_get_expr(ad.adbin, ad.adrelid), \
                       BOOL_OR(COALESCE(i.indisprimary, false)), \
                       BOOL_OR(COALESCE(i.indisunique AND NOT i.indisprimary AND i.indnkeyatts = 1 AND a.attnum = i.indkey[0], false)), \
                       pg_catalog.col_description(c.oid, a.attnum) \
                       FROM pg_catalog.pg_attribute a \
                       JOIN pg_catalog.pg_class c ON c.oid = a.attrelid \
                       JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                       JOIN pg_catalog.pg_type t ON t.oid = a.atttypid \
                       LEFT JOIN pg_catalog.pg_attrdef ad ON ad.adrelid = a.attrelid AND ad.adnum = a.attnum \
                       LEFT JOIN pg_catalog.pg_index i ON i.indrelid = c.oid AND a.attnum = ANY(i.indkey) \
                       WHERE n.nspname = $1 AND c.relname = $2 AND a.attnum > 0 AND NOT a.attisdropped \
                       GROUP BY c.oid, a.attnum, a.attname, a.atttypid, a.atttypmod, t.typname, a.attnotnull, ad.adbin, ad.adrelid \
                       ORDER BY a.attnum ASC";
        let start = Instant::now();
        let col_rows = self
            .client
            .query(col_sql, &[&schema, &table])
            .await
            .map_err(|e| self.map_aux_query_err(e))?;

        let fk_sql = "SELECT a_src.attname, con.conname, nf.nspname, cf.relname, a_dst.attname \
                      FROM pg_catalog.pg_constraint con \
                      JOIN pg_catalog.pg_class cs ON cs.oid = con.conrelid \
                      JOIN pg_catalog.pg_namespace ns ON ns.oid = cs.relnamespace \
                      JOIN pg_catalog.pg_class cf ON cf.oid = con.confrelid \
                      JOIN pg_catalog.pg_namespace nf ON nf.oid = cf.relnamespace \
                      JOIN LATERAL unnest(con.conkey, con.confkey) WITH ORDINALITY AS u(src_att, dst_att, ord) ON true \
                      JOIN pg_catalog.pg_attribute a_src ON a_src.attrelid = con.conrelid AND a_src.attnum = u.src_att \
                      JOIN pg_catalog.pg_attribute a_dst ON a_dst.attrelid = con.confrelid AND a_dst.attnum = u.dst_att \
                      WHERE con.contype = 'f' AND ns.nspname = $1 AND cs.relname = $2 \
                      ORDER BY con.conname ASC, u.ord ASC";
        let fk_rows = self
            .client
            .query(fk_sql, &[&schema, &table])
            .await
            .map_err(|e| self.map_aux_query_err(e))?;

        let mut fks: HashMap<String, ForeignKeyRef> = HashMap::new();
        for row in fk_rows {
            let col_name: String = row.get(0);
            fks.entry(col_name).or_insert_with(|| ForeignKeyRef {
                constraint_name: row.get(1),
                target_schema: row.get(2),
                target_table: row.get(3),
                target_column: row.get(4),
            });
        }

        let columns = col_rows
            .into_iter()
            .map(|row| {
                let name: String = row.get(0);
                let foreign_key = fks.remove(&name);
                ColumnMeta {
                    name,
                    data_type: row.get(1),
                    udt_name: row.get(2),
                    nullable: row.get(3),
                    default_expr: row.get(4),
                    is_primary_key: row.get(5),
                    is_unique: row.get(6),
                    comment: row.get(7),
                    foreign_key,
                }
            })
            .collect();

        let log = ExecutedSql {
            sql: format!(
                "{col_sql} -- $1 = '{}', $2 = '{}'",
                schema.replace('\'', "''"),
                table.replace('\'', "''")
            ),
            duration_ms: start.elapsed().as_millis() as u64,
            error: None,
        };
        Ok((columns, log))
    }
}

#[async_trait]
impl DbSession for PostgresSession {
    fn engine(&self) -> DbEngine {
        DbEngine::Postgres
    }

    fn server_info(&self) -> &ServerInfo {
        &self.server_info
    }

    fn postgres_session_mode(&self) -> Option<crate::sql::PostgresSessionMode> {
        let live_scs = self.standard_conforming_strings.load(Ordering::Acquire);
        Some(self.current_postgres_session_mode(live_scs))
    }

    fn is_disconnected(&self) -> bool {
        self.disconnected.load(Ordering::Acquire) || self.client.is_closed()
    }

    async fn list_databases(&self) -> Result<(Vec<DatabaseInfo>, ExecutedSql), DbError> {
        let _guard = self.client_lock.lock().await;
        self.ensure_alive()?;
        let sql = "SELECT d.datname, COALESCE(pg_catalog.pg_get_userbyid(d.datdba), ''), \
                   pg_catalog.pg_database_size(d.datname) \
                   FROM pg_catalog.pg_database d \
                   WHERE d.datistemplate = false \
                   ORDER BY d.datname ASC";
        let start = Instant::now();
        let rows = match self.client.query(sql, &[]).await {
            Ok(rows) => rows,
            Err(err) if is_insufficient_privilege(&err) => {
                self.record_pg_aux_query_error(&err);
                let fallback_sql = "SELECT d.datname, COALESCE(pg_catalog.pg_get_userbyid(d.datdba), ''), NULL::bigint \
                                    FROM pg_catalog.pg_database d \
                                    WHERE d.datistemplate = false \
                                    ORDER BY d.datname ASC";
                let rows = self
                    .client
                    .query(fallback_sql, &[])
                    .await
                    .map_err(|e| self.map_aux_query_err(e))?;
                let dbs = rows
                    .into_iter()
                    .map(|row| DatabaseInfo {
                        name: row.get(0),
                        owner: row.get(1),
                        size_bytes: row.get(2),
                    })
                    .collect();
                return Ok((
                    dbs,
                    ExecutedSql {
                        sql: fallback_sql.to_owned(),
                        duration_ms: start.elapsed().as_millis() as u64,
                        error: None,
                    },
                ));
            }
            Err(err) => return Err(self.map_aux_query_err(err)),
        };

        let dbs = rows
            .into_iter()
            .map(|row| DatabaseInfo {
                name: row.get(0),
                owner: row.get(1),
                size_bytes: row.get(2),
            })
            .collect();
        Ok((
            dbs,
            ExecutedSql {
                sql: sql.to_owned(),
                duration_ms: start.elapsed().as_millis() as u64,
                error: None,
            },
        ))
    }

    async fn list_schemas(&self) -> Result<(Vec<SchemaInfo>, ExecutedSql), DbError> {
        let _guard = self.client_lock.lock().await;
        self.ensure_alive()?;
        let sql = "SELECT nspname FROM pg_catalog.pg_namespace \
                   WHERE nspname NOT LIKE 'pg_temp_%' AND nspname NOT LIKE 'pg_toast_temp_%' \
                   ORDER BY (nspname = 'public') DESC, \
                            (nspname LIKE 'pg_%' OR nspname = 'information_schema') ASC, \
                            nspname ASC";
        let start = Instant::now();
        let rows = self
            .client
            .query(sql, &[])
            .await
            .map_err(|e| self.map_aux_query_err(e))?;
        let schemas = rows
            .into_iter()
            .map(|row| {
                let name: String = row.get(0);
                let is_system = name.starts_with("pg_") || name == "information_schema";
                SchemaInfo { name, is_system }
            })
            .collect();
        Ok((
            schemas,
            ExecutedSql {
                sql: sql.to_owned(),
                duration_ms: start.elapsed().as_millis() as u64,
                error: None,
            },
        ))
    }

    async fn list_objects(&self, schema: &str) -> Result<(Vec<DbObject>, ExecutedSql), DbError> {
        let _guard = self.client_lock.lock().await;
        self.ensure_alive()?;
        let sql = "SELECT c.relname, c.relkind::text, \
                   CASE WHEN c.reltuples >= 0 THEN c.reltuples::bigint ELSE NULL END, \
                   CASE WHEN c.relkind IN ('r', 'p', 'm') THEN pg_catalog.pg_total_relation_size(c.oid) ELSE NULL END \
                   FROM pg_catalog.pg_class c \
                   JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                   WHERE n.nspname = $1 AND c.relkind IN ('r', 'p', 'v', 'm', 'f') \
                   ORDER BY c.relname ASC";
        let start = Instant::now();
        let rows = self
            .client
            .query(sql, &[&schema])
            .await
            .map_err(|e| self.map_aux_query_err(e))?;
        let objects = rows
            .into_iter()
            .filter_map(|row| {
                let name: String = row.get(0);
                let relkind: String = row.get(1);
                let kind = match relkind.as_str() {
                    "r" | "p" => DbObjectKind::Table,
                    "v" => DbObjectKind::View,
                    "m" => DbObjectKind::MaterializedView,
                    "f" => DbObjectKind::ForeignTable,
                    _ => return None,
                };
                Some(DbObject {
                    schema: schema.to_owned(),
                    name,
                    kind,
                    estimated_rows: row.get(2),
                    total_bytes: row.get(3),
                })
            })
            .collect();
        Ok((
            objects,
            ExecutedSql {
                sql: format!("{sql} -- $1 = '{}'", schema.replace('\'', "''")),
                duration_ms: start.elapsed().as_millis() as u64,
                error: None,
            },
        ))
    }

    async fn list_columns(&self, schema: &str, object: &str) -> Result<(Vec<ColumnSummary>, ExecutedSql), DbError> {
        let _guard = self.client_lock.lock().await;
        self.ensure_alive()?;
        let sql = "SELECT a.attname, pg_catalog.format_type(a.atttypid, a.atttypmod), \
                   NOT a.attnotnull, BOOL_OR(COALESCE(i.indisprimary, false)) \
                   FROM pg_catalog.pg_attribute a \
                   JOIN pg_catalog.pg_class c ON c.oid = a.attrelid \
                   JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                   LEFT JOIN pg_catalog.pg_index i ON i.indrelid = c.oid AND i.indisprimary AND a.attnum = ANY(i.indkey) \
                   WHERE n.nspname = $1 AND c.relname = $2 AND a.attnum > 0 AND NOT a.attisdropped \
                   GROUP BY a.attnum, a.attname, a.atttypid, a.atttypmod, a.attnotnull \
                   ORDER BY a.attnum ASC";
        let start = Instant::now();
        let rows = self
            .client
            .query(sql, &[&schema, &object])
            .await
            .map_err(|e| self.map_aux_query_err(e))?;
        let cols = rows
            .into_iter()
            .map(|row| ColumnSummary {
                name: row.get(0),
                data_type: row.get(1),
                nullable: row.get(2),
                is_primary_key: row.get(3),
            })
            .collect();
        Ok((
            cols,
            ExecutedSql {
                sql: format!(
                    "{sql} -- $1 = '{}', $2 = '{}'",
                    schema.replace('\'', "''"),
                    object.replace('\'', "''")
                ),
                duration_ms: start.elapsed().as_millis() as u64,
                error: None,
            },
        ))
    }

    async fn ping(&self) -> Result<(), DbError> {
        let _guard = self.client_lock.lock().await;
        self.ensure_alive()?;
        match tokio::time::timeout(PING_TIMEOUT, self.client.check_connection()).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(err)) => Err(self.map_aux_query_err(err)),
            Err(_) => {
                self.disconnected.store(true, Ordering::Release);
                Err(DbError::Disconnected)
            }
        }
    }

    async fn fetch_table_page(&self, req: &PageRequest) -> Result<(TablePage, Vec<ExecutedSql>), DbError> {
        let _guard = self.client_lock.lock().await;
        self.ensure_alive()?;
        let limit = req.limit.clamp(1, 5000);
        let offset = req.offset.max(0);
        let (columns, _) = self.fetch_column_metas(&req.schema, &req.table).await?;

        if columns.is_empty() {
            return Ok((
                TablePage {
                    columns,
                    rows: Vec::new(),
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

        let sql = build_table_page_sql(&req.schema, &req.table, &columns, req, limit, offset);
        let before_scs = self.sync_standard_conforming_strings().await;
        let start = Instant::now();
        let pg_rows = self
            .client
            .query(&sql, &[])
            .await
            .map_err(|e| self.map_aux_query_err(e))?;
        let after_scs = self.sync_standard_conforming_strings().await;
        if after_scs != before_scs
            && let Ok(mut guard) = self.live_state.lock()
        {
            if guard.tx_state == Some(crate::sql::PostgresTxState::Idle) {
                guard.session_scs = Some(after_scs);
                guard.pre_tx_scs = Some(after_scs);
            } else {
                guard.session_scs = None;
            }
        }
        let elapsed_ms = start.elapsed().as_millis() as u64;

        let mut logs = vec![ExecutedSql {
            sql,
            duration_ms: elapsed_ms,
            error: None,
        }];

        let has_more = pg_rows.len() > limit as usize;
        let row_count = pg_rows.len().min(limit as usize);
        let mut decoded_rows = Vec::with_capacity(row_count);
        for row in pg_rows.into_iter().take(limit as usize) {
            let mut cells = Vec::with_capacity(columns.len());
            for (idx, col) in columns.iter().enumerate() {
                let raw = row
                    .try_get::<usize, Option<String>>(idx)
                    .map_err(|e| self.map_session_err(e))?;
                cells.push(CellValue::parse_from_pg_text(&col.udt_name, raw));
            }
            decoded_rows.push(cells);
        }

        let filter_active = req
            .filter_where
            .as_deref()
            .is_some_and(|w| !w.trim().is_empty());

        let (total_rows_estimate, total_is_exact) = if !has_more {
            (Some(offset + decoded_rows.len() as i64), true)
        } else if !filter_active {
            let est_sql = "SELECT CASE WHEN c.reltuples >= 0 THEN c.reltuples::bigint ELSE NULL END \
                           FROM pg_catalog.pg_class c \
                           JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                           WHERE n.nspname = $1 AND c.relname = $2";
            let est_start = Instant::now();
            let est = match self.client.query_opt(est_sql, &[&req.schema, &req.table]).await {
                Ok(Some(r)) => r.get::<usize, Option<i64>>(0),
                Ok(None) => None,
                Err(err) => {
                    self.record_pg_aux_query_error(&err);
                    None
                }
            };
            logs.push(ExecutedSql {
                sql: format!(
                    "{est_sql} -- $1 = '{}', $2 = '{}'",
                    req.schema.replace('\'', "''"),
                    req.table.replace('\'', "''")
                ),
                duration_ms: est_start.elapsed().as_millis() as u64,
                error: None,
            });
            (est, false)
        } else {
            (None, false)
        };

        Ok((
            TablePage {
                columns,
                rows: decoded_rows,
                offset,
                limit,
                total_rows_estimate,
                total_is_exact,
                has_more,
                elapsed_ms,
            },
            logs,
        ))
    }

    async fn describe_table(&self, schema: &str, table: &str) -> Result<(TableStructure, Vec<ExecutedSql>), DbError> {
        let _guard = self.client_lock.lock().await;
        self.ensure_alive()?;
        let mut logs = Vec::new();
        let (columns, col_log) = self.fetch_column_metas(schema, table).await?;
        logs.push(col_log);

        let idx_sql = "SELECT \
                       ic.relname, \
                       i.indisunique, \
                       i.indisprimary, \
                       am.amname, \
                       CASE WHEN tc.relkind IN ('r', 'p', 'm') THEN pg_catalog.pg_relation_size(ic.oid) ELSE NULL END, \
                       pg_catalog.pg_get_indexdef(ic.oid), \
                       ARRAY( \
                           SELECT COALESCE(a.attname::text, '<expr>') \
                           FROM unnest(i.indkey) WITH ORDINALITY AS k(attnum, ord) \
                           LEFT JOIN pg_catalog.pg_attribute a \
                               ON a.attrelid = tc.oid AND a.attnum = k.attnum AND k.attnum > 0 \
                           ORDER BY k.ord \
                       ) \
                       FROM pg_catalog.pg_index i \
                       JOIN pg_catalog.pg_class ic ON ic.oid = i.indexrelid \
                       JOIN pg_catalog.pg_class tc ON tc.oid = i.indrelid \
                       JOIN pg_catalog.pg_namespace n ON n.oid = tc.relnamespace \
                       JOIN pg_catalog.pg_am am ON am.oid = ic.relam \
                       WHERE n.nspname = $1 AND tc.relname = $2 \
                       ORDER BY i.indisprimary DESC, i.indisunique DESC, ic.relname ASC";
        let idx_start = Instant::now();
        let idx_rows = self
            .client
            .query(idx_sql, &[&schema, &table])
            .await
            .map_err(|e| self.map_aux_query_err(e))?;
        logs.push(ExecutedSql {
            sql: format!(
                "{idx_sql} -- $1 = '{}', $2 = '{}'",
                schema.replace('\'', "''"),
                table.replace('\'', "''")
            ),
            duration_ms: idx_start.elapsed().as_millis() as u64,
            error: None,
        });

        let indexes: Vec<IndexInfo> = idx_rows
            .into_iter()
            .map(|row| IndexInfo {
                name: row.get(0),
                is_unique: row.get(1),
                is_primary: row.get(2),
                method: row.get(3),
                size_bytes: row.get(4),
                definition: row.get(5),
                columns: row.get(6),
            })
            .collect();

        let stats_sql = "SELECT \
                         c.relkind::text, \
                         CASE WHEN c.relkind IN ('r', 'p', 'm') THEN pg_catalog.pg_total_relation_size(c.oid) ELSE NULL END, \
                         CASE WHEN c.relkind IN ('r', 'p', 'm') THEN pg_catalog.pg_table_size(c.oid) ELSE NULL END, \
                         CASE WHEN c.relkind IN ('r', 'p', 'm') THEN pg_catalog.pg_indexes_size(c.oid) ELSE NULL END, \
                         CASE WHEN c.reltuples >= 0 THEN c.reltuples::bigint ELSE NULL END, \
                         pg_catalog.obj_description(c.oid, 'pg_class'), \
                         CASE WHEN c.relkind IN ('v', 'm') THEN pg_catalog.pg_get_viewdef(c.oid, true) ELSE NULL END \
                         FROM pg_catalog.pg_class c \
                         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                         WHERE n.nspname = $1 AND c.relname = $2";
        let stats_start = Instant::now();
        let stats_row = self
            .client
            .query_opt(stats_sql, &[&schema, &table])
            .await
            .map_err(|e| self.map_aux_query_err(e))?;
        logs.push(ExecutedSql {
            sql: format!(
                "{stats_sql} -- $1 = '{}', $2 = '{}'",
                schema.replace('\'', "''"),
                table.replace('\'', "''")
            ),
            duration_ms: stats_start.elapsed().as_millis() as u64,
            error: None,
        });

        let stats = if let Some(row) = stats_row {
            let relkind: String = row.get(0);
            let total_bytes: Option<i64> = row.get(1);
            let data_bytes: Option<i64> = row.get(2);
            let index_bytes: Option<i64> = row.get(3);
            let estimated_rows: Option<i64> = row.get(4);
            let comment: Option<String> = row.get(5);
            let view_def: Option<String> = row.get(6);

            let ddl = match relkind.as_str() {
                "v" => format!(
                    "CREATE OR REPLACE VIEW {}.{} AS\n{};",
                    quote_ident(schema),
                    quote_ident(table),
                    view_def.as_deref().unwrap_or("").trim()
                ),
                "m" => format!(
                    "CREATE MATERIALIZED VIEW {}.{} AS\n{};",
                    quote_ident(schema),
                    quote_ident(table),
                    view_def.as_deref().unwrap_or("").trim()
                ),
                _ => synthesize_table_ddl(schema, table, &columns, &indexes),
            };
            TableStats {
                total_bytes,
                data_bytes,
                index_bytes,
                estimated_rows,
                comment,
                ddl,
            }
        } else {
            TableStats {
                total_bytes: None,
                data_bytes: None,
                index_bytes: None,
                estimated_rows: None,
                comment: None,
                ddl: synthesize_table_ddl(schema, table, &columns, &indexes),
            }
        };

        Ok((
            TableStructure {
                columns,
                indexes,
                stats,
            },
            logs,
        ))
    }

    async fn execute_sql(
        &self,
        statements: &[String],
        row_limit: Option<usize>,
    ) -> Result<(QueryBatchResult, Vec<ExecutedSql>), DbError> {
        self.execute_sql_inner(statements, row_limit, 0, None, None).await
    }

    async fn execute_sql_cancellable(
        &self,
        statements: &[String],
        row_limit: Option<usize>,
        cancel_key: u64,
        cancel_flag: Option<&AtomicBool>,
    ) -> Result<(QueryBatchResult, Vec<ExecutedSql>), DbError> {
        self.execute_sql_inner(statements, row_limit, cancel_key, cancel_flag, None)
            .await
    }

    async fn execute_sql_cancellable_checked(
        &self,
        statements: &[String],
        row_limit: Option<usize>,
        cancel_key: u64,
        cancel_flag: Option<&AtomicBool>,
        expected_sql_mode: crate::sql::SqlSessionMode,
    ) -> Result<(QueryBatchResult, Vec<ExecutedSql>), DbError> {
        self.execute_sql_inner(
            statements,
            row_limit,
            cancel_key,
            cancel_flag,
            expected_sql_mode.postgres,
        )
        .await
    }

    async fn cancel_running_query(&self) -> Result<(), DbError> {
        {
            let mut state = self.cancel_state.lock().map_err(|_| DbError::Disconnected)?;
            if state.cancel_requested {
                return Ok(());
            }
            if self.is_disconnected() {
                return Err(DbError::Disconnected);
            }
            state.cancel_requested = true;
            state.in_flight_cancels += 1;
            self.cancel_in_flight_tx.send_replace(state.in_flight_cancels);
        }
        let _in_flight = InFlightCancelGuard {
            state: &self.cancel_state,
            tx: &self.cancel_in_flight_tx,
        };
        self.send_cancel_request().await
    }

    async fn cancel_query_by_key(&self, cancel_key: u64) -> Result<(), DbError> {
        {
            let mut state = self.cancel_state.lock().map_err(|_| DbError::Disconnected)?;
            if state.active_key != cancel_key {
                return Ok(());
            }
            if state.cancel_requested {
                return Ok(());
            }
            if self.is_disconnected() {
                return Err(DbError::Disconnected);
            }
            state.cancel_requested = true;
            state.in_flight_cancels += 1;
            self.cancel_in_flight_tx.send_replace(state.in_flight_cancels);
        }
        let _in_flight = InFlightCancelGuard {
            state: &self.cancel_state,
            tx: &self.cancel_in_flight_tx,
        };
        self.send_cancel_request().await
    }

    async fn execute_transaction(
        &self,
        statements: &[String],
    ) -> Result<(TransactionOutcome, Vec<ExecutedSql>), DbError> {
        if statements.is_empty() {
            return Err(DbError::InvalidInput("no SQL statements to execute in transaction".to_owned()));
        }
        let _guard = self.client_lock.lock().await;
        self.ensure_alive()
            .map_err(|e| DbError::DisconnectedBeforeDispatch(Box::new(e)))?;
        let total_start = Instant::now();
        let mut logs = Vec::with_capacity(statements.len() + 2);
        let mut total_rows_affected = 0u64;
        let mut executed = 0usize;

        let begin_start = Instant::now();
        if let Err(pg_err) = self.client.simple_query("BEGIN").await {
            let mapped = self.map_aux_query_err(pg_err);
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

        for raw_stmt in statements {
            let stmt = crate::sql::trim_sql_whitespace_for_engine(DbEngine::Postgres, raw_stmt);
            if stmt.is_empty() {
                continue;
            }
            let stmt_start = Instant::now();
            match self.client.simple_query(stmt).await {
                Ok(messages) => {
                    for msg in messages {
                        if let SimpleQueryMessage::CommandComplete(affected) = msg {
                            total_rows_affected = total_rows_affected.saturating_add(affected);
                        }
                    }
                    executed += 1;
                    logs.push(ExecutedSql {
                        sql: stmt.to_owned(),
                        duration_ms: stmt_start.elapsed().as_millis() as u64,
                        error: None,
                    });
                }
                Err(pg_err) => {
                    let mapped = self.map_session_err(pg_err);
                    drop(self.client.simple_query("ROLLBACK").await);
                    let after_scs = self.sync_standard_conforming_strings().await;
                    self.record_pg_transaction_ended(after_scs);
                    return Err(mapped);
                }
            }
        }

        let commit_start = Instant::now();
        match self.client.simple_query("COMMIT").await {
            Ok(_) => {
                let after_scs = self.sync_standard_conforming_strings().await;
                self.record_pg_transaction_ended(after_scs);
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
            Err(pg_err) => {
                let mapped = self.map_session_err(pg_err);
                drop(self.client.simple_query("ROLLBACK").await);
                let after_scs = self.sync_standard_conforming_strings().await;
                self.record_pg_transaction_ended(after_scs);
                Err(mapped)
            }
        }
    }

    async fn explain_sql(&self, statement: &str, analyze: bool) -> Result<(ExplainPlan, ExecutedSql), DbError> {
        self.explain_sql_inner(statement, analyze, None).await
    }

    async fn explain_sql_checked(
        &self,
        statement: &str,
        analyze: bool,
        expected_sql_mode: crate::sql::SqlSessionMode,
    ) -> Result<(ExplainPlan, ExecutedSql), DbError> {
        self.explain_sql_inner(statement, analyze, expected_sql_mode.postgres)
            .await
    }

    async fn fetch_fk_row(
        &self,
        fk: &ForeignKeyRef,
        value: &CellValue,
        source_col: &ColumnMeta,
    ) -> Result<(FkPeekRow, ExecutedSql), DbError> {
        let _guard = self.client_lock.lock().await;
        self.ensure_alive()?;
        let Some(where_clause) = build_fk_where_clause(&fk.target_column, value, source_col) else {
            return Err(DbError::InvalidInput("cannot peek NULL foreign-key value".to_owned()));
        };

        let (target_columns, _) = self
            .fetch_column_metas(&fk.target_schema, &fk.target_table)
            .await?;
        if target_columns.is_empty() {
            return Err(DbError::InvalidInput(format!(
                "target table {}.{} has no columns",
                fk.target_schema, fk.target_table
            )));
        }

        let req = PageRequest {
            schema: fk.target_schema.clone(),
            table: fk.target_table.clone(),
            filter_where: Some(where_clause.clone()),
            sort: None,
            limit: 1,
            offset: 0,
        };
        let sql = build_table_page_sql(&fk.target_schema, &fk.target_table, &target_columns, &req, 1, 0);
        let start = Instant::now();
        let pg_rows = self
            .client
            .query(&sql, &[])
            .await
            .map_err(|e| self.map_aux_query_err(e))?;
        let elapsed_ms = start.elapsed().as_millis() as u64;

        let row = if let Some(first) = pg_rows.into_iter().next() {
            let mut cells = Vec::with_capacity(target_columns.len());
            for (idx, col) in target_columns.iter().enumerate() {
                let raw = first
                    .try_get::<usize, Option<String>>(idx)
                    .map_err(|e| self.map_session_err(e))?;
                cells.push(CellValue::parse_from_pg_text(&col.udt_name, raw));
            }
            Some(cells)
        } else {
            None
        };

        Ok((
            FkPeekRow {
                fk: fk.clone(),
                lookup_display: format!("{} = {}", fk.target_column, value.display_compact()),
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
    }
}

fn should_retire_after_cancel(cancel_requested: bool, server_observed_cancel: bool) -> bool {
    cancel_requested && !server_observed_cancel
}

fn is_pg_multi_command_prepare_error(err: &tokio_postgres::Error) -> bool {
    err.as_db_error().is_some_and(|db| {
        db.code().code() == "42601"
            && db
                .message()
                .to_ascii_lowercase()
                .contains("cannot insert multiple commands into a prepared statement")
    })
}

fn evaluate_mid_batch_pg_mode(
    live_scs: bool,
    prev_live_scs: bool,
    expected_active: Option<bool>,
    next_stmt: &str,
) -> Result<(), String> {
    if expected_active.is_some_and(|exp| live_scs != exp)
        || (expected_active.is_none() && live_scs != prev_live_scs)
    {
        return Err(
            "PostgreSQL string escaping mode changed mid-batch; remaining statements skipped for safety"
                .to_owned(),
        );
    }
    if crate::sql::postgres_slice_has_multiple_statements(next_stmt, live_scs) {
        return Err(
            "PostgreSQL statement contains multiple semicolon-separated statements under the live string escaping mode; remaining statements skipped for safety"
                .to_owned(),
        );
    }
    Ok(())
}

impl PostgresSession {
    fn retire_connection(&self) {
        self.disconnected.store(true, Ordering::Release);
        if let Ok(mut guard) = self.conn_task.lock()
            && let Some(handle) = guard.take()
        {
            handle.abort();
        }
    }

    async fn send_cancel_request(&self) -> Result<(), DbError> {
        let token = self.client.cancel_token();
        if !self.use_tls_for_cancel {
            match tokio::time::timeout(PING_TIMEOUT, token.cancel_query(NoTls)).await {
                Ok(Ok(())) => return Ok(()),
                Ok(Err(err)) => return Err(map_pg_err(err)),
                Err(_) => return Err(DbError::ConnectionFailed("cancel request timed out".to_owned())),
            }
        }
        let tls_cfg = build_rustls_config(&self.ssl)?;
        let connector = MakeRustlsConnect::new(tls_cfg);
        match tokio::time::timeout(PING_TIMEOUT, token.cancel_query(connector)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(err)) if self.ssl.mode == SslMode::Prefer && err.as_db_error().is_none() => {
                match tokio::time::timeout(PING_TIMEOUT, token.cancel_query(NoTls)).await {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(err)) => Err(map_pg_err(err)),
                    Err(_) => Err(DbError::ConnectionFailed("cancel request timed out".to_owned())),
                }
            }
            Ok(Err(err)) => Err(map_pg_err(err)),
            Err(_) => Err(DbError::ConnectionFailed("cancel request timed out".to_owned())),
        }
    }

    async fn explain_sql_inner(
        &self,
        statement: &str,
        analyze: bool,
        expected_pg_mode: Option<crate::sql::PostgresSessionMode>,
    ) -> Result<(ExplainPlan, ExecutedSql), DbError> {
        let _guard = self.client_lock.lock().await;
        self.ensure_alive()
            .map_err(|e| DbError::DisconnectedBeforeDispatch(Box::new(e)))?;
        let trimmed = crate::sql::trim_sql_whitespace_for_engine(DbEngine::Postgres, statement);
        if trimmed.is_empty() {
            return Err(DbError::InvalidInput("cannot explain an empty SQL statement".to_owned()));
        }
        let live_scs = self.sync_standard_conforming_strings().await;
        if let Some(ref expected) = expected_pg_mode {
            if &self.current_postgres_session_mode(live_scs) != expected {
                return Err(DbError::PostgresStringModeMismatch);
            }
            if crate::sql::postgres_slice_has_multiple_statements(trimmed, live_scs) {
                return Err(DbError::InvalidInput(
                    "cannot EXPLAIN a PostgreSQL slice containing multiple statements; select or split a single statement"
                        .to_owned(),
                ));
            }
        }
        let explain_query = build_explain_sql(trimmed, analyze);
        if expected_pg_mode.is_some()
            && let Err(prep_err) = self.client.prepare(&explain_query).await
            && is_pg_multi_command_prepare_error(&prep_err)
        {
            let after_scs = self.sync_standard_conforming_strings().await;
            self.record_pg_stmt_error(&explain_query, live_scs, &prep_err, true, after_scs);
            return Err(DbError::InvalidInput(
                "cannot EXPLAIN a PostgreSQL slice containing multiple statements; select or split a single statement"
                    .to_owned(),
            ));
        }
        let start = Instant::now();
        let messages = match self.client.simple_query(&explain_query).await {
            Ok(msgs) => msgs,
            Err(err) => {
                let after_scs = self.sync_standard_conforming_strings().await;
                self.record_pg_stmt_error(&explain_query, live_scs, &err, false, after_scs);
                return Err(self.map_session_err(err));
            }
        };
        let after_scs = self.sync_standard_conforming_strings().await;
        if analyze {
            self.record_pg_stmt_success(trimmed, live_scs, after_scs);
        } else if after_scs != live_scs {
            self.record_pg_stmt_success(&explain_query, live_scs, after_scs);
        }
        let duration_ms = start.elapsed().as_millis() as u64;

        let mut chunks = Vec::new();
        for msg in messages {
            if let SimpleQueryMessage::Row(row) = msg
                && let Some(cell) = row.get(0)
            {
                chunks.push(cell.to_owned());
            }
        }
        let raw_json = chunks.join("\n");
        let plan = ExplainPlan::parse_json(trimmed.to_owned(), analyze, &raw_json)
            .map_err(|e| DbError::InvalidInput(e.to_string()))?;

        Ok((
            plan,
            ExecutedSql {
                sql: explain_query,
                duration_ms,
                error: None,
            },
        ))
    }

    async fn execute_sql_inner(
        &self,
        statements: &[String],
        row_limit: Option<usize>,
        cancel_key: u64,
        cancel_flag: Option<&AtomicBool>,
        expected_pg_mode: Option<crate::sql::PostgresSessionMode>,
    ) -> Result<(QueryBatchResult, Vec<ExecutedSql>), DbError> {
        let is_cancelled = || cancel_flag.is_some_and(|f| f.load(Ordering::Acquire));
        if is_cancelled() {
            return Err(DbError::Cancelled);
        }
        let _guard = self.client_lock.lock().await;
        if is_cancelled() {
            return Err(DbError::Cancelled);
        }
        self.ensure_alive()
            .map_err(|e| DbError::DisconnectedBeforeDispatch(Box::new(e)))?;
        let mut live_scs = self.sync_standard_conforming_strings().await;
        if let Some(ref expected) = expected_pg_mode {
            if &self.current_postgres_session_mode(live_scs) != expected {
                return Err(DbError::PostgresStringModeMismatch);
            }
            if let Some(first_stmt) = statements
                .iter()
                .map(|s| crate::sql::trim_sql_whitespace_for_engine(DbEngine::Postgres, s))
                .find(|s| !s.is_empty())
                && crate::sql::postgres_slice_has_multiple_statements(first_stmt, live_scs)
            {
                return Err(DbError::InvalidInput(
                    "PostgreSQL statement contains multiple semicolon-separated statements; split statements before executing"
                        .to_owned(),
                ));
            }
        }
        {
            let mut state = self.cancel_state.lock().map_err(|_| DbError::Disconnected)?;
            state.active_key = cancel_key;
            state.cancel_requested = false;
        }
        let active_guard = ActiveCancelKeyGuard(&self.cancel_state);
        let mut server_observed_cancel = false;
        let mut expected_tracker = expected_pg_mode.as_ref().map(crate::sql::PgScsTracker::from_mode);
        let mut prev_live_scs = live_scs;
        let mut dispatched_any = false;

        let outcome: Result<(QueryBatchResult, Vec<ExecutedSql>), DbError> = async {
            if is_cancelled() {
                return Err(DbError::Cancelled);
            }

            let batch_start = Instant::now();
            let mut results = Vec::new();
            let mut logs = Vec::new();
            let mut cancelled = false;

            for raw_stmt in statements {
                let stmt = crate::sql::trim_sql_whitespace_for_engine(DbEngine::Postgres, raw_stmt);
                if stmt.is_empty() {
                    continue;
                }
                if is_cancelled() {
                    if results.is_empty() {
                        return Err(DbError::Cancelled);
                    }
                    cancelled = true;
                    break;
                }
                if dispatched_any
                    && let Some(ref tracker) = expected_tracker
                {
                    live_scs = self.sync_standard_conforming_strings().await;
                    if let Err(err_msg) =
                        evaluate_mid_batch_pg_mode(live_scs, prev_live_scs, tracker.active, stmt)
                    {
                        logs.push(ExecutedSql {
                            sql: stmt.to_owned(),
                            duration_ms: 0,
                            error: Some(err_msg.clone()),
                        });
                        results.push(QueryStatementResult {
                            sql: stmt.to_owned(),
                            columns: Vec::new(),
                            rows: Vec::new(),
                            rows_affected: None,
                            truncated: false,
                            total_rows_seen: 0,
                            elapsed_ms: 0,
                            error: Some(err_msg),
                        });
                        break;
                    }
                }
                let start = Instant::now();
                let prepared_cols: Option<Vec<(String, String)>> =
                    match self.client.prepare(stmt).await {
                        Ok(s) => Some(
                            s.columns()
                                .iter()
                                .map(|c| (c.name().to_owned(), c.type_().name().to_owned()))
                                .collect(),
                        ),
                        Err(prep_err)
                            if expected_pg_mode.is_some()
                                && is_pg_multi_command_prepare_error(&prep_err) =>
                        {
                            let after_scs = self.sync_standard_conforming_strings().await;
                            self.record_pg_stmt_error(stmt, live_scs, &prep_err, true, after_scs);
                            if results.is_empty() {
                                return Err(DbError::InvalidInput(
                                    "PostgreSQL statement contains multiple commands; split statements before executing"
                                        .to_owned(),
                                ));
                            }
                            let err_msg =
                                "PostgreSQL statement contains multiple commands; remaining statements skipped for safety"
                                    .to_owned();
                            let elapsed_ms = start.elapsed().as_millis() as u64;
                            logs.push(ExecutedSql {
                                sql: stmt.to_owned(),
                                duration_ms: elapsed_ms,
                                error: Some(err_msg.clone()),
                            });
                            results.push(QueryStatementResult {
                                sql: stmt.to_owned(),
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
                        Err(_) => None,
                    };
                if is_cancelled() {
                    if results.is_empty() {
                        return Err(DbError::Cancelled);
                    }
                    cancelled = true;
                    break;
                }

                match self.client.simple_query(stmt).await {
                    Ok(messages) => {
                        let after_scs = self.sync_standard_conforming_strings().await;
                        self.record_pg_stmt_success(stmt, live_scs, after_scs);
                        if let Some(ref mut tracker) = expected_tracker {
                            tracker.apply(stmt);
                        }
                        prev_live_scs = after_scs;
                        live_scs = after_scs;
                        dispatched_any = true;
                        let elapsed_ms = start.elapsed().as_millis() as u64;
                        let mut columns: Vec<ColumnMeta> = Vec::new();
                        let mut rows: Vec<Vec<CellValue>> = Vec::new();
                        let mut rows_affected: Option<u64> = None;
                        let mut truncated = false;
                        let mut total_rows_seen = 0usize;

                        for msg in messages {
                            match msg {
                                SimpleQueryMessage::RowDescription(desc) => {
                                    columns = desc
                                        .iter()
                                        .enumerate()
                                        .map(|(idx, col)| {
                                            let udt_name = prepared_cols
                                                .as_ref()
                                                .and_then(|pc| pc.get(idx))
                                                .filter(|(_, _)| prepared_cols.as_ref().is_some_and(|p| p.len() == desc.len()))
                                                .map_or_else(|| "text".to_owned(), |(_, ty)| ty.clone());
                                            ColumnMeta {
                                                name: col.name().to_owned(),
                                                data_type: human_pg_type(&udt_name).to_owned(),
                                                udt_name,
                                                nullable: true,
                                                default_expr: None,
                                                is_primary_key: false,
                                                is_unique: false,
                                                comment: None,
                                                foreign_key: None,
                                            }
                                        })
                                        .collect();
                                }
                                SimpleQueryMessage::Row(row) => {
                                    total_rows_seen += 1;
                                    if row_limit.is_none_or(|lim| rows.len() < lim) {
                                        let mut decoded = Vec::with_capacity(columns.len());
                                        for (idx, col) in columns.iter().enumerate() {
                                            let val = row.get(idx).map(str::to_owned);
                                            decoded.push(CellValue::parse_from_pg_text(&col.udt_name, val));
                                        }
                                        rows.push(decoded);
                                    } else {
                                        truncated = true;
                                    }
                                }
                                SimpleQueryMessage::CommandComplete(affected) => {
                                    rows_affected = Some(affected);
                                }
                                _ => {}
                            }
                        }

                        logs.push(ExecutedSql {
                            sql: stmt.to_owned(),
                            duration_ms: elapsed_ms,
                            error: None,
                        });
                        results.push(QueryStatementResult {
                            sql: stmt.to_owned(),
                            columns,
                            rows,
                            rows_affected,
                            truncated,
                            total_rows_seen,
                            elapsed_ms,
                            error: None,
                        });
                    }
                    Err(pg_err) => {
                        let after_scs = self.sync_standard_conforming_strings().await;
                        self.record_pg_stmt_error(stmt, live_scs, &pg_err, false, after_scs);
                        let elapsed_ms = start.elapsed().as_millis() as u64;
                        let mapped = self.map_session_err(pg_err);
                        if matches!(mapped, DbError::Disconnected) && results.is_empty() {
                            return Err(DbError::Disconnected);
                        }
                        if matches!(mapped, DbError::Cancelled) {
                            server_observed_cancel = true;
                            cancelled = true;
                        }
                        let err_msg = mapped.to_string();
                        logs.push(ExecutedSql {
                            sql: stmt.to_owned(),
                            duration_ms: elapsed_ms,
                            error: Some(err_msg.clone()),
                        });
                        results.push(QueryStatementResult {
                            sql: stmt.to_owned(),
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
        }
        .await;

        let had_cancel = if let Ok(mut state) = self.cancel_state.lock() {
            state.active_key = 0;
            state.cancel_requested
        } else {
            false
        };
        drop(active_guard);

        let mut rx = self.cancel_in_flight_tx.subscribe();
        drop(rx.wait_for(|&n| n == 0).await);

        if should_retire_after_cancel(had_cancel, server_observed_cancel) {
            self.retire_connection();
        } else {
            let _ = self.sync_standard_conforming_strings().await;
        }

        outcome
    }
}

fn human_pg_type(udt_name: &str) -> &str {
    match udt_name {
        "int2" => "smallint",
        "int4" => "integer",
        "int8" => "bigint",
        "float4" => "real",
        "float8" => "double precision",
        "bool" => "boolean",
        "timestamptz" => "timestamp with time zone",
        "timestamp" => "timestamp without time zone",
        "timetz" => "time with time zone",
        "_text" => "text[]",
        "_int4" => "integer[]",
        "_int8" => "bigint[]",
        "_uuid" => "uuid[]",
        other => other,
    }
}

/// Quotes a PostgreSQL SQL identifier safely by doubling embedded double quotes.
pub fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// Builds the paginated `SELECT` query with safe identifier quoting and text/hex projection.
pub fn build_table_page_sql(
    schema: &str,
    table: &str,
    columns: &[ColumnMeta],
    req: &PageRequest,
    limit: i64,
    offset: i64,
) -> String {
    let mut projections = String::new();
    for (idx, col) in columns.iter().enumerate() {
        if idx > 0 {
            projections.push_str(", ");
        }
        let quoted = quote_ident(&col.name);
        if col.udt_name == "bytea" {
            projections.push_str(&format!("pg_catalog.encode({quoted}, 'hex')"));
        } else {
            projections.push_str(&format!("{quoted}::text"));
        }
    }

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

/// Synthesizes a readable PostgreSQL `CREATE TABLE` DDL statement from columns, constraints, and indexes.
pub fn synthesize_table_ddl(
    schema: &str,
    table: &str,
    columns: &[ColumnMeta],
    indexes: &[IndexInfo],
) -> String {
    let mut out = format!("CREATE TABLE {}.{} (\n", quote_ident(schema), quote_ident(table));
    let mut clauses: Vec<String> = Vec::with_capacity(columns.len() + 4);

    for col in columns {
        let mut line = format!("    {} {}", quote_ident(&col.name), col.data_type);
        if !col.nullable {
            line.push_str(" NOT NULL");
        }
        if let Some(def) = &col.default_expr {
            line.push_str(&format!(" DEFAULT {def}"));
        }
        clauses.push(line);
    }

    let pk_cols: Vec<String> = columns
        .iter()
        .filter(|c| c.is_primary_key)
        .map(|c| quote_ident(&c.name))
        .collect();
    if !pk_cols.is_empty() {
        clauses.push(format!("    PRIMARY KEY ({})", pk_cols.join(", ")));
    }

    for col in columns {
        if let Some(fk) = &col.foreign_key {
            clauses.push(format!(
                "    CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {}.{} ({})",
                quote_ident(&fk.constraint_name),
                quote_ident(&col.name),
                quote_ident(&fk.target_schema),
                quote_ident(&fk.target_table),
                quote_ident(&fk.target_column),
            ));
        }
    }

    out.push_str(&clauses.join(",\n"));
    out.push_str("\n);");

    let extra_indexes: Vec<&IndexInfo> = indexes.iter().filter(|idx| !idx.is_primary).collect();
    if !extra_indexes.is_empty() {
        out.push('\n');
        for idx in extra_indexes {
            out.push('\n');
            out.push_str(idx.definition.trim());
            if !idx.definition.trim_end().ends_with(';') {
                out.push(';');
            }
        }
    }
    out
}

fn build_pg_config(
    profile: &ConnectionProfile,
    password: Option<&str>,
    database: &str,
) -> (Config, bool) {
    let mut cfg = Config::new();
    let host = profile.host.trim();
    cfg.host(host);
    if profile.port > 0 {
        cfg.port(profile.port);
    }
    if !host.starts_with('/') {
        cfg.keepalives(true);
        cfg.keepalives_idle(Duration::from_secs(60));
        cfg.keepalives_interval(Duration::from_secs(10));
        cfg.keepalives_retries(3);
    }
    cfg.user(profile.user.trim());
    if profile.password_mode != PasswordMode::None
        && let Some(pw) = password
        && !pw.is_empty()
    {
        cfg.password(pw);
    }
    cfg.dbname(database);
    cfg.connect_timeout(CONNECT_TIMEOUT);
    cfg.application_name("TableView");

    let (pg_ssl_mode, use_tls) = match profile.ssl.mode {
        SslMode::Disable => (tokio_postgres::config::SslMode::Disable, false),
        SslMode::Prefer if host.starts_with('/') => (tokio_postgres::config::SslMode::Disable, false),
        SslMode::Prefer => (tokio_postgres::config::SslMode::Prefer, true),
        SslMode::Require | SslMode::VerifyCa | SslMode::VerifyFull => {
            (tokio_postgres::config::SslMode::Require, true)
        }
    };
    cfg.ssl_mode(pg_ssl_mode);
    (cfg, use_tls)
}

type RawPgConnect = (
    tokio_postgres::Client,
    JoinHandle<()>,
    Arc<AtomicBool>,
    Arc<AtomicBool>,
    tokio::sync::mpsc::UnboundedSender<tokio::sync::oneshot::Sender<bool>>,
    bool,
);

async fn connect_raw(
    profile: &ConnectionProfile,
    password: Option<&str>,
    database: &str,
) -> Result<RawPgConnect, DbError> {
    let (mut cfg, use_tls) = build_pg_config(profile, password, database);

    if !use_tls {
        let (c, h, d, scs, tx) = connect_no_tls(&cfg).await?;
        return Ok((c, h, d, scs, tx, false));
    }

    let tls_cfg = build_rustls_config(&profile.ssl)?;
    let connector = MakeRustlsConnect::new(tls_cfg);
    match tokio::time::timeout(CONNECT_TIMEOUT, cfg.connect(connector)).await {
        Ok(Ok((client, connection))) => {
            let (handle, disconnected, scs, tx) = spawn_connection_driver(connection);
            Ok((client, handle, disconnected, scs, tx, true))
        }
        Ok(Err(err)) => {
            if profile.ssl.mode == SslMode::Prefer && err.as_db_error().is_none() {
                cfg.ssl_mode(tokio_postgres::config::SslMode::Disable);
                let (c, h, d, scs, tx) = connect_no_tls(&cfg).await?;
                Ok((c, h, d, scs, tx, false))
            } else {
                Err(map_pg_err(err))
            }
        }
        Err(_) => Err(DbError::ConnectionFailed("connection timed out".to_owned())),
    }
}

async fn connect_no_tls(
    cfg: &Config,
) -> Result<
    (
        tokio_postgres::Client,
        JoinHandle<()>,
        Arc<AtomicBool>,
        Arc<AtomicBool>,
        tokio::sync::mpsc::UnboundedSender<tokio::sync::oneshot::Sender<bool>>,
    ),
    DbError,
> {
    match tokio::time::timeout(CONNECT_TIMEOUT, cfg.connect(NoTls)).await {
        Ok(Ok((client, connection))) => {
            let (handle, disconnected, scs, tx) = spawn_connection_driver(connection);
            Ok((client, handle, disconnected, scs, tx))
        }
        Ok(Err(err)) => Err(map_pg_err(err)),
        Err(_) => Err(DbError::ConnectionFailed("connection timed out".to_owned())),
    }
}

struct PgConnectionDriver<S, T> {
    connection: tokio_postgres::Connection<S, T>,
    disconnected: Arc<AtomicBool>,
    standard_conforming_strings: Arc<AtomicBool>,
    param_sync_rx: tokio::sync::mpsc::UnboundedReceiver<tokio::sync::oneshot::Sender<bool>>,
    sync_closed: bool,
}

impl<S, T> Future for PgConnectionDriver<S, T>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<()> {
        let this = self.get_mut();
        let mut terminal = false;
        loop {
            match this.connection.poll_message(cx) {
                Poll::Ready(Some(Ok(_))) => {
                    let scs = this
                        .connection
                        .parameter("standard_conforming_strings")
                        .is_none_or(|v| !v.eq_ignore_ascii_case("off"));
                    this.standard_conforming_strings.store(scs, Ordering::Release);
                }
                Poll::Ready(None | Some(Err(_))) => {
                    terminal = true;
                    break;
                }
                Poll::Pending => break,
            }
        }

        let scs = this
            .connection
            .parameter("standard_conforming_strings")
            .is_none_or(|v| !v.eq_ignore_ascii_case("off"));
        this.standard_conforming_strings.store(scs, Ordering::Release);

        while !this.sync_closed {
            match this.param_sync_rx.poll_recv(cx) {
                Poll::Ready(Some(reply)) => {
                    if reply.send(scs).is_err() {
                        continue;
                    }
                }
                Poll::Ready(None) => {
                    this.sync_closed = true;
                }
                Poll::Pending => break,
            }
        }

        if terminal {
            this.disconnected.store(true, Ordering::Release);
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

fn spawn_connection_driver<S, T>(
    connection: tokio_postgres::Connection<S, T>,
) -> (
    JoinHandle<()>,
    Arc<AtomicBool>,
    Arc<AtomicBool>,
    tokio::sync::mpsc::UnboundedSender<tokio::sync::oneshot::Sender<bool>>,
)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let disconnected = Arc::new(AtomicBool::new(false));
    let initial_scs = connection
        .parameter("standard_conforming_strings")
        .is_none_or(|v| !v.eq_ignore_ascii_case("off"));
    let standard_conforming_strings = Arc::new(AtomicBool::new(initial_scs));
    let (param_sync_tx, param_sync_rx) = tokio::sync::mpsc::unbounded_channel();
    let driver = PgConnectionDriver {
        connection,
        disconnected: Arc::clone(&disconnected),
        standard_conforming_strings: Arc::clone(&standard_conforming_strings),
        param_sync_rx,
        sync_closed: false,
    };
    let handle = tokio::spawn(driver);
    (handle, disconnected, standard_conforming_strings, param_sync_tx)
}

pub(crate) fn build_rustls_config(ssl: &SslConfig) -> Result<ClientConfig, DbError> {
    ensure_crypto_provider();
    let provider = Arc::new(rustls::crypto::ring::default_provider());

    let builder = match ssl.mode {
        SslMode::Disable => return Err(DbError::TlsConfig("TLS disabled".to_owned())),
        SslMode::Prefer | SslMode::Require => ClientConfig::builder_with_provider(Arc::clone(&provider))
            .with_safe_default_protocol_versions()
            .map_err(|e| DbError::TlsConfig(e.to_string()))?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerifyServerCert { provider })),
        SslMode::VerifyCa => {
            let (roots, anchors) = load_roots_and_anchors(ssl.ca_cert_path.as_deref())?;
            let _ = roots;
            ClientConfig::builder_with_provider(Arc::clone(&provider))
                .with_safe_default_protocol_versions()
                .map_err(|e| DbError::TlsConfig(e.to_string()))?
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(CaOnlyServerCertVerifier { provider, anchors }))
        }
        SslMode::VerifyFull => {
            let (roots, _) = load_roots_and_anchors(ssl.ca_cert_path.as_deref())?;
            ClientConfig::builder_with_provider(Arc::clone(&provider))
                .with_safe_default_protocol_versions()
                .map_err(|e| DbError::TlsConfig(e.to_string()))?
                .with_root_certificates(roots)
        }
    };

    match (&ssl.client_cert_path, &ssl.client_key_path) {
        (Some(cert_path), Some(key_path)) => {
            let certs = load_certs(cert_path)?;
            let key = load_private_key(key_path)?;
            builder
                .with_client_auth_cert(certs, key)
                .map_err(|e| DbError::TlsConfig(e.to_string()))
        }
        _ => Ok(builder.with_no_client_auth()),
    }
}

fn load_roots_and_anchors(ca_path: Option<&Path>) -> Result<(RootCertStore, Vec<TrustAnchor<'static>>), DbError> {
    let mut store = RootCertStore::empty();
    let mut anchors = Vec::new();
    if let Some(path) = ca_path {
        let certs = load_certs(path)?;
        if certs.is_empty() {
            return Err(DbError::TlsConfig(format!(
                "no certificates found in CA file {}",
                path.display()
            )));
        }
        for cert in certs {
            let anchor = anchor_from_trusted_cert(&cert)
                .map_err(|e| DbError::TlsConfig(format!("invalid CA cert {}: {e}", path.display())))?
                .to_owned();
            anchors.push(anchor);
            store
                .add(cert)
                .map_err(|e| DbError::TlsConfig(format!("invalid CA cert {}: {e}", path.display())))?;
        }
        return Ok((store, anchors));
    }

    for anchor in webpki_roots::TLS_SERVER_ROOTS {
        anchors.push(anchor.to_owned());
    }
    store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    let native = rustls_native_certs::load_native_certs();
    for cert in native.certs {
        if let Ok(anchor) = anchor_from_trusted_cert(&cert) {
            anchors.push(anchor.to_owned());
        }
        drop(store.add(cert));
    }

    Ok((store, anchors))
}

fn load_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>, DbError> {
    CertificateDer::pem_file_iter(path)
        .map_err(|e| map_pem_error("parse PEM certs", "open", path, e))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| map_pem_error("parse PEM certs", "open", path, e))
}

fn load_private_key(path: &Path) -> Result<PrivateKeyDer<'static>, DbError> {
    PrivateKeyDer::from_pem_file(path).map_err(|e| match e {
        PemError::NoItemsFound => DbError::TlsConfig(format!("no private key found in {}", path.display())),
        other => map_pem_error("parse private key", "open", path, other),
    })
}

fn map_pem_error(parse_ctx: &str, open_ctx: &str, path: &Path, error: PemError) -> DbError {
    match error {
        PemError::Io(io_err) => DbError::TlsConfig(format!("{open_ctx} {}: {io_err}", path.display())),
        other => DbError::TlsConfig(format!("{parse_ctx} {}: {other}", path.display())),
    }
}

#[derive(Debug)]
struct NoVerifyServerCert {
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for NoVerifyServerCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[derive(Debug)]
struct CaOnlyServerCertVerifier {
    provider: Arc<CryptoProvider>,
    anchors: Vec<TrustAnchor<'static>>,
}

impl ServerCertVerifier for CaOnlyServerCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        let cert = EndEntityCert::try_from(end_entity)
            .map_err(|_| TlsError::InvalidCertificate(rustls::CertificateError::BadEncoding))?;
        cert.verify_for_usage(
            self.provider.signature_verification_algorithms.all,
            &self.anchors,
            intermediates,
            now,
            KeyUsage::server_auth(),
            None,
            None,
        )
        .map_err(|e| TlsError::InvalidCertificate(rustls::CertificateError::Other(rustls::OtherError(Arc::new(e)))))?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn is_missing_database_error(err: &DbError) -> bool {
    matches!(err, DbError::QueryFailed { code: Some(code), .. } if code == "3D000")
}

fn is_insufficient_privilege(err: &tokio_postgres::Error) -> bool {
    err.as_db_error()
        .is_some_and(|db| db.code().code() == "42501")
}

fn is_pg_disconnect_sqlstate(code: &str) -> bool {
    matches!(code, "57P01" | "57P02" | "57P03") || code.starts_with("08")
}

fn map_pg_err(err: tokio_postgres::Error) -> DbError {
    if let Some(db) = err.as_db_error() {
        let code = db.code().code().to_owned();
        if code == "28P01" || code == "28000" {
            return DbError::AuthFailed(db.message().to_owned());
        }
        if code == "57014" {
            return DbError::Cancelled;
        }
        return DbError::QueryFailed {
            message: db.message().to_owned(),
            code: Some(code),
            detail: db.detail().map(str::to_owned),
            hint: db.hint().map(str::to_owned),
        };
    }
    if err.is_closed() {
        return DbError::Disconnected;
    }
    DbError::ConnectionFailed(err.to_string())
}

/// Extracts a concise version string such as `"PostgreSQL 18.4"` from `SELECT version()`.
pub fn concise_pg_version(raw: &str) -> String {
    let mut parts = raw.split_whitespace();
    match (parts.next(), parts.next()) {
        (Some(name), Some(ver)) if name.eq_ignore_ascii_case("postgresql") => {
            format!("PostgreSQL {ver}")
        }
        _ => raw.split(',').next().unwrap_or(raw).trim().to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use crate::connection::{ConnectionProfile, SslMode};

    use super::{
        PgLiveSessionState, PgSqlstateError, build_pg_config, concise_pg_version,
        evaluate_mid_batch_pg_mode, is_pg_disconnect_sqlstate, should_retire_after_cancel,
    };

    #[test]
    fn build_pg_config_sets_expected_ssl_mode_for_every_profile_mode() {
        let mut profile = ConnectionProfile::new_postgres("greendale_pg", "127.0.0.1", "troy");

        profile.ssl.mode = SslMode::Disable;
        let (cfg, use_tls) = build_pg_config(&profile, None, "postgres");
        assert_eq!(cfg.get_ssl_mode(), tokio_postgres::config::SslMode::Disable);
        assert!(!use_tls);

        profile.ssl.mode = SslMode::Prefer;
        let (cfg, use_tls) = build_pg_config(&profile, None, "postgres");
        assert_eq!(cfg.get_ssl_mode(), tokio_postgres::config::SslMode::Prefer);
        assert!(use_tls);

        let mut socket_profile = ConnectionProfile::new_postgres("greendale_pg_sock", "/tmp", "troy");
        socket_profile.ssl.mode = SslMode::Prefer;
        socket_profile.port = 55432;
        let (cfg, use_tls) = build_pg_config(&socket_profile, None, "postgres");
        assert_eq!(cfg.get_ssl_mode(), tokio_postgres::config::SslMode::Disable);
        assert_eq!(cfg.get_ports(), &[55432]);
        assert!(!use_tls);

        socket_profile.port = 0;
        let (cfg, use_tls) = build_pg_config(&socket_profile, None, "postgres");
        assert_eq!(cfg.get_ssl_mode(), tokio_postgres::config::SslMode::Disable);
        assert!(cfg.get_ports().is_empty());
        assert!(!use_tls);

        for mode in [SslMode::Require, SslMode::VerifyCa, SslMode::VerifyFull] {
            profile.ssl.mode = mode;
            let (cfg, use_tls) = build_pg_config(&profile, None, "postgres");
            assert_eq!(cfg.get_ssl_mode(), tokio_postgres::config::SslMode::Require);
            assert!(use_tls);
        }
    }

    #[test]
    fn parses_concise_postgresql_version() {
        assert_eq!(
            concise_pg_version("PostgreSQL 18.4 (Postgres.app) on aarch64-apple-darwin23.6.0, compiled by Apple clang"),
            "PostgreSQL 18.4"
        );
        assert_eq!(concise_pg_version("PostgreSQL 14.8 on x86_64-pc-linux-gnu"), "PostgreSQL 14.8");
    }

    #[test]
    fn classifies_pg_disconnect_sqlstates() {
        for code in ["57P01", "57P02", "57P03", "08006", "08000"] {
            assert!(is_pg_disconnect_sqlstate(code), "{code} should indicate disconnect");
        }
        for code in ["42601", "28P01", "57014", "23505"] {
            assert!(!is_pg_disconnect_sqlstate(code), "{code} should not indicate disconnect");
        }
    }

    #[test]
    fn retires_postgres_connection_only_when_cancel_not_confirmed_consumed() {
        assert!(should_retire_after_cancel(true, false));
        assert!(!should_retire_after_cancel(true, true));
        assert!(!should_retire_after_cancel(false, false));
        assert!(!should_retire_after_cancel(false, true));
    }

    #[test]
    fn pem_loader_reports_missing_file_and_empty_key() {
        let Ok(dir) = tempfile::tempdir() else {
            panic!("create temp dir");
        };
        let empty = dir.path().join("empty.pem");
        if let Err(err) = std::fs::write(&empty, b"not a pem block") {
            panic!("write empty pem: {err}");
        }

        let Err(err) = super::load_private_key(&empty) else {
            panic!("expected missing private key error");
        };
        assert!(matches!(err, crate::engine::DbError::TlsConfig(msg) if msg.contains("no private key found")));

        let missing = dir.path().join("missing.pem");
        let Err(missing_err) = super::load_certs(&missing) else {
            panic!("expected missing cert file error");
        };
        assert!(matches!(missing_err, crate::engine::DbError::TlsConfig(msg) if msg.contains("open ")));
    }

    #[test]
    fn evaluates_mid_batch_pg_mode_drift_and_single_statement_bounds() {
        assert!(evaluate_mid_batch_pg_mode(true, true, Some(true), "SELECT 1").is_ok());
        assert!(evaluate_mid_batch_pg_mode(false, false, Some(false), "SELECT 'Troy\\'; 1'").is_ok());
        assert!(
            evaluate_mid_batch_pg_mode(true, true, Some(true), "SELECT 'a\\' AS first; SELECT 'b'").is_err(),
            "multi-statement slice under live scs=true must be rejected mid-batch"
        );
        assert!(
            evaluate_mid_batch_pg_mode(false, true, None, "SELECT 2").is_err(),
            "unexpected server scs drift under uncertain mode must abort remaining statements"
        );
        assert!(
            evaluate_mid_batch_pg_mode(true, true, Some(false), "SELECT 2").is_err(),
            "mismatch against expected tracker mode must abort remaining statements"
        );
    }

    #[test]
    fn tracks_pg_live_session_state_across_transactions_savepoints_reset_and_errors() {
        use crate::sql::{PgScsTracker, PostgresSavepointMode, PostgresTxState};

        // 1. Cross-run BEGIN -> SET LOCAL -> SAVEPOINT -> ROLLBACK TO SAVEPOINT -> COMMIT.
        let mut st = PgLiveSessionState::new(true, Some(false));
        st.apply_success("BEGIN", true, true);
        assert_eq!(st.tx_state, Some(PostgresTxState::InTransaction));
        st.apply_success("SET LOCAL standard_conforming_strings = off", true, false);
        assert_eq!(
            st.to_session_mode(false).tx_state,
            Some(PostgresTxState::InTransaction)
        );
        st.apply_success("SAVEPOINT sp1", false, false);
        st.apply_success("SET LOCAL standard_conforming_strings = on", false, true);
        let mid_mode = st.to_session_mode(true);
        assert_eq!(
            mid_mode.savepoints,
            vec![PostgresSavepointMode {
                name: "sp1".to_owned(),
                active_standard_conforming_strings: Some(false),
                session_standard_conforming_strings: Some(true),
            }]
        );
        let mut mid_tracker = PgScsTracker::from_mode(&mid_mode);
        mid_tracker.apply("ROLLBACK TO SAVEPOINT sp1");
        assert_eq!(mid_tracker.active, Some(false));
        st.apply_success("ROLLBACK TO SAVEPOINT sp1", true, false);
        assert_eq!(st.to_session_mode(false).session_standard_conforming_strings, Some(true));
        st.apply_success("COMMIT", false, true);
        let committed = st.to_session_mode(true);
        assert_eq!(committed.tx_state, Some(PostgresTxState::Idle));
        assert!(!committed.has_untracked_savepoints);
        assert!(committed.savepoints.is_empty());
        assert_eq!(committed.session_standard_conforming_strings, Some(true));

        // 2. Configured RESET value = off and AND CHAIN preservation.
        st.apply_success("BEGIN", true, true);
        st.apply_success("SET standard_conforming_strings = on", true, true);
        st.apply_success("RESET standard_conforming_strings", true, false);
        assert_eq!(st.reset_scs, Some(false));
        st.apply_success("COMMIT AND CHAIN", false, false);
        assert_eq!(st.tx_state, Some(PostgresTxState::InTransaction));
        st.apply_success("ROLLBACK AND CHAIN", false, false);
        assert_eq!(st.tx_state, Some(PostgresTxState::InTransaction));
        st.apply_success("ROLLBACK", false, false);
        assert_eq!(st.tx_state, Some(PostgresTxState::Idle));

        // 3. CALL / DO mark transaction state uncertain; SQLSTATE 25P02 & 25P01 update tx state.
        st.apply_success("CALL greendale_proc()", false, false);
        assert_eq!(st.tx_state, None);
        st.apply_sqlstate_error(
            "SELECT 1",
            false,
            PgSqlstateError {
                code: Some("25P02"),
                ..Default::default()
            },
            false,
        );
        assert_eq!(st.tx_state, Some(PostgresTxState::Aborted));
        st.apply_sqlstate_error(
            "ROLLBACK",
            false,
            PgSqlstateError {
                code: Some("25P01"),
                ..Default::default()
            },
            false,
        );
        assert_eq!(st.tx_state, Some(PostgresTxState::Idle));

        // 4. SQL error inside transaction aborts block while preserving pre_tx_scs; ROLLBACK TO recovers.
        let mut abort_st = PgLiveSessionState::new(true, Some(true));
        abort_st.apply_success("BEGIN", true, true);
        abort_st.apply_success("SET standard_conforming_strings = off", true, false);
        abort_st.apply_success("SAVEPOINT greendale", false, false);
        abort_st.apply_sqlstate_error(
            "SELECT 1/0",
            false,
            PgSqlstateError {
                code: Some("22012"),
                ..Default::default()
            },
            false,
        );
        let aborted_mode = abort_st.to_session_mode(false);
        assert_eq!(aborted_mode.tx_state, Some(PostgresTxState::Aborted));
        assert_eq!(aborted_mode.pre_tx_standard_conforming_strings, Some(true));
        let mut aborted_tracker = PgScsTracker::from_mode(&aborted_mode);
        aborted_tracker.apply("COMMIT");
        assert_eq!(aborted_tracker.active, Some(true));
        assert_eq!(aborted_tracker.tx_state, Some(PostgresTxState::Idle));

        let mut recover_tracker = PgScsTracker::from_mode(&aborted_mode);
        recover_tracker.apply("ROLLBACK TO SAVEPOINT greendale");
        assert_eq!(recover_tracker.active, Some(false));
        assert_eq!(recover_tracker.tx_state, Some(PostgresTxState::InTransaction));

        abort_st.apply_success("COMMIT", false, true);
        assert_eq!(abort_st.tx_state, Some(PostgresTxState::Idle));
        assert_eq!(abort_st.session_scs, Some(true));

        // 5. Same-active-scs session change during tx (e.g. EXPLAIN ANALYZE SELECT set_config) updates session_scs.
        let mut explain_st = PgLiveSessionState::new(true, Some(true));
        explain_st.apply_success("BEGIN", true, true);
        explain_st.apply_success("SET LOCAL standard_conforming_strings = off", true, false);
        explain_st.apply_success(
            "SELECT set_config('standard_conforming_strings', 'off', false)",
            false,
            false,
        );
        assert_eq!(explain_st.tx_state, Some(PostgresTxState::InTransaction));
        assert_eq!(explain_st.session_scs, Some(false));

        // 6. Failed COMMIT (deferred constraint / serialization) returns PostgreSQL to Idle.
        let mut deferred_st = PgLiveSessionState::new(true, Some(true));
        deferred_st.apply_success("BEGIN", true, true);
        deferred_st.apply_sqlstate_error(
            "COMMIT",
            true,
            PgSqlstateError {
                code: Some("23505"),
                ..Default::default()
            },
            true,
        );
        assert_eq!(deferred_st.tx_state, Some(PostgresTxState::Idle));
        assert_eq!(deferred_st.session_scs, Some(true));
        let mut after_deferred_tracker = PgScsTracker::from_mode(&deferred_st.to_session_mode(true));
        after_deferred_tracker.apply("SET standard_conforming_strings = off");
        assert_eq!(after_deferred_tracker.active, Some(false));

        // Deferred trigger raising 42601 on COMMIT rolls back to Idle; top-level syntax 42601 keeps Aborted.
        let mut deferred_42601_st = PgLiveSessionState::new(true, Some(true));
        deferred_42601_st.apply_success("BEGIN", true, true);
        deferred_42601_st.apply_success("SET LOCAL standard_conforming_strings = off", true, false);
        deferred_42601_st.apply_sqlstate_error(
            "COMMIT",
            false,
            PgSqlstateError {
                code: Some("42601"),
                has_error_context: true,
                ..Default::default()
            },
            true,
        );
        assert_eq!(deferred_42601_st.tx_state, Some(PostgresTxState::Idle));
        assert_eq!(deferred_42601_st.session_scs, Some(true));
        let mut after_42601_tracker =
            PgScsTracker::from_mode(&deferred_42601_st.to_session_mode(true));
        after_42601_tracker.apply("SET standard_conforming_strings = off");
        assert_eq!(after_42601_tracker.active, Some(false));

        // Syntax error on COMMIT (42601) keeps transaction Aborted; COMMIT AND CHAIN error returns Idle;
        // failed PREPARE TRANSACTION fails closed.
        let mut syntax_commit_st = PgLiveSessionState::new(true, Some(true));
        syntax_commit_st.apply_success("BEGIN", true, true);
        syntax_commit_st.apply_sqlstate_error(
            "COMMIT FOOBAR",
            true,
            PgSqlstateError {
                code: Some("42601"),
                is_top_level_syntax_error: true,
                ..Default::default()
            },
            true,
        );
        assert_eq!(syntax_commit_st.tx_state, Some(PostgresTxState::Aborted));

        let mut prep_syntax_commit_st = PgLiveSessionState::new(true, Some(true));
        prep_syntax_commit_st.apply_success("BEGIN", true, true);
        prep_syntax_commit_st.apply_sqlstate_error(
            "COMMIT",
            true,
            PgSqlstateError {
                code: Some("42601"),
                is_top_level_syntax_error: true,
                ..Default::default()
            },
            true,
        );
        assert_eq!(prep_syntax_commit_st.tx_state, Some(PostgresTxState::Aborted));

        let mut chain_fail_st = PgLiveSessionState::new(true, Some(true));
        chain_fail_st.apply_success("BEGIN", true, true);
        chain_fail_st.apply_sqlstate_error(
            "COMMIT AND CHAIN",
            true,
            PgSqlstateError {
                code: Some("23505"),
                ..Default::default()
            },
            true,
        );
        assert_eq!(chain_fail_st.tx_state, Some(PostgresTxState::Idle));
        assert_eq!(chain_fail_st.session_scs, Some(true));
        let mut after_chain_tracker = PgScsTracker::from_mode(&chain_fail_st.to_session_mode(true));
        assert_eq!(after_chain_tracker.tx_state, Some(PostgresTxState::Idle));
        after_chain_tracker.apply("SET standard_conforming_strings = off");
        assert_eq!(after_chain_tracker.active, Some(false));

        let mut chain_42601_st = PgLiveSessionState::new(true, Some(true));
        chain_42601_st.apply_success("BEGIN", true, true);
        chain_42601_st.apply_sqlstate_error(
            "COMMIT AND CHAIN",
            false,
            PgSqlstateError {
                code: Some("42601"),
                has_error_context: true,
                ..Default::default()
            },
            true,
        );
        assert_eq!(chain_42601_st.tx_state, Some(PostgresTxState::Idle));
        assert_eq!(chain_42601_st.session_scs, Some(true));

        // Deferred triggers raising 3B001 or 25P02 during COMMIT/COMMIT AND CHAIN also return Idle,
        // while ROLLBACK TO SAVEPOINT missing (3B001 without error context) still marks Aborted.
        let mut deferred_3b001_st = PgLiveSessionState::new(true, Some(true));
        deferred_3b001_st.apply_success("BEGIN", true, true);
        deferred_3b001_st.apply_success("SET LOCAL standard_conforming_strings = off", true, false);
        deferred_3b001_st.apply_sqlstate_error(
            "COMMIT",
            false,
            PgSqlstateError {
                code: Some("3B001"),
                has_error_context: true,
                ..Default::default()
            },
            true,
        );
        assert_eq!(deferred_3b001_st.tx_state, Some(PostgresTxState::Idle));
        assert_eq!(deferred_3b001_st.session_scs, Some(true));

        let mut deferred_25p02_chain_st = PgLiveSessionState::new(true, Some(true));
        deferred_25p02_chain_st.apply_success("BEGIN", true, true);
        deferred_25p02_chain_st.apply_sqlstate_error(
            "COMMIT AND CHAIN",
            false,
            PgSqlstateError {
                code: Some("25P02"),
                has_error_context: true,
                ..Default::default()
            },
            true,
        );
        assert_eq!(deferred_25p02_chain_st.tx_state, Some(PostgresTxState::Idle));

        let mut bad_sp_3b001_st = PgLiveSessionState::new(true, Some(true));
        bad_sp_3b001_st.apply_success("BEGIN", true, true);
        bad_sp_3b001_st.apply_sqlstate_error(
            "ROLLBACK TO SAVEPOINT missing",
            false,
            PgSqlstateError {
                code: Some("3B001"),
                ..Default::default()
            },
            true,
        );
        assert_eq!(bad_sp_3b001_st.tx_state, Some(PostgresTxState::Aborted));

        // Standalone DO block raising 3B001 / 25P02 / 25P01 carries error context and must not trigger
        // transaction-state shortcuts.
        let mut do_3b001_st = PgLiveSessionState::new(true, Some(true));
        do_3b001_st.apply_sqlstate_error(
            "DO $$ BEGIN RAISE SQLSTATE '3B001'; END $$",
            true,
            PgSqlstateError {
                code: Some("3B001"),
                has_error_context: true,
                ..Default::default()
            },
            true,
        );
        assert_eq!(do_3b001_st.tx_state, Some(PostgresTxState::Idle));
        let mut after_do_3b001 = PgScsTracker::from_mode(&do_3b001_st.to_session_mode(true));
        after_do_3b001.apply("BEGIN");
        after_do_3b001.apply("SET LOCAL standard_conforming_strings = off");
        assert_eq!(after_do_3b001.active, Some(false));
        assert_eq!(after_do_3b001.tx_state, Some(PostgresTxState::InTransaction));

        let mut do_25p02_st = PgLiveSessionState::new(true, Some(true));
        do_25p02_st.apply_sqlstate_error(
            "DO $$ BEGIN RAISE SQLSTATE '25P02'; END $$",
            true,
            PgSqlstateError {
                code: Some("25P02"),
                has_error_context: true,
                ..Default::default()
            },
            true,
        );
        assert_eq!(do_25p02_st.tx_state, Some(PostgresTxState::Idle));

        let mut do_25p01_in_tx_st = PgLiveSessionState::new(true, Some(true));
        do_25p01_in_tx_st.apply_success("BEGIN", true, true);
        do_25p01_in_tx_st.apply_sqlstate_error(
            "DO $$ BEGIN RAISE SQLSTATE '25P01'; END $$",
            true,
            PgSqlstateError {
                code: Some("25P01"),
                has_error_context: true,
                ..Default::default()
            },
            true,
        );
        assert_eq!(do_25p01_in_tx_st.tx_state, Some(PostgresTxState::Aborted));

        let mut unknown_do_3b001_st = PgLiveSessionState::new(true, Some(true));
        unknown_do_3b001_st.tx_state = None;
        unknown_do_3b001_st.apply_sqlstate_error(
            "DO $$ BEGIN RAISE SQLSTATE '3B001'; END $$",
            true,
            PgSqlstateError {
                code: Some("3B001"),
                has_error_context: true,
                ..Default::default()
            },
            true,
        );
        assert_eq!(unknown_do_3b001_st.tx_state, None);

        let mut prep_fail_st = PgLiveSessionState::new(true, Some(true));
        prep_fail_st.apply_success("BEGIN", true, true);
        prep_fail_st.apply_sqlstate_error(
            "PREPARE TRANSACTION 'gid'",
            true,
            PgSqlstateError {
                code: Some("0A000"),
                ..Default::default()
            },
            true,
        );
        assert_eq!(prep_fail_st.tx_state, None);

        // 7. Savepoint identifiers truncate to 63 bytes so prefix-colliding names match newer snapshot.
        let prefix = "g".repeat(60);
        let sp1 = format!("{prefix}abc_first");
        let sp2 = format!("{prefix}abc_second");
        let mut long_sp_st = PgLiveSessionState::new(true, Some(true));
        long_sp_st.apply_success("BEGIN", true, true);
        long_sp_st.apply_success(&format!("SAVEPOINT {sp1}"), true, true);
        long_sp_st.apply_success("SET LOCAL standard_conforming_strings = off", true, false);
        long_sp_st.apply_success(&format!("SAVEPOINT {sp2}"), false, false);
        long_sp_st.apply_success("SET LOCAL standard_conforming_strings = on", false, true);
        let mut long_sp_tracker = PgScsTracker::from_mode(&long_sp_st.to_session_mode(true));
        long_sp_tracker.apply(&format!("ROLLBACK TO SAVEPOINT {sp1}"));
        assert_eq!(long_sp_tracker.active, Some(false));

        // 8. LATIN1 SingleByte encoding keeps 32x 'é' + 'a'/'b' (33 bytes) distinct, handles >=63 ASCII prefix
        // before 'é', and clears stale tracked savepoints when an untracked non-ASCII savepoint occurs.
        let sp_lat1 = format!("\"{}a\"", "é".repeat(32));
        let sp_lat2 = format!("\"{}b\"", "é".repeat(32));
        let mut latin1_st = PgLiveSessionState::new(true, Some(true));
        latin1_st.server_encoding = Some(crate::sql::PostgresServerEncoding::from_pg_name("LATIN1"));
        latin1_st.apply_success("BEGIN", true, true);
        latin1_st.apply_success(&format!("SAVEPOINT {sp_lat1}"), true, true);
        latin1_st.apply_success("SET LOCAL standard_conforming_strings = off", true, false);
        latin1_st.apply_success(&format!("SAVEPOINT {sp_lat2}"), false, false);
        latin1_st.apply_success("SET LOCAL standard_conforming_strings = on", false, true);
        let mut latin1_tracker = PgScsTracker::from_mode(&latin1_st.to_session_mode(true));
        latin1_tracker.apply(&format!("ROLLBACK TO SAVEPOINT {sp_lat1}"));
        assert_eq!(latin1_tracker.active, Some(true));
        let mut latin1_tracker2 = PgScsTracker::from_mode(&latin1_st.to_session_mode(true));
        latin1_tracker2.apply(&format!("ROLLBACK TO SAVEPOINT {sp_lat2}"));
        assert_eq!(latin1_tracker2.active, Some(false));

        let sp_ascii_63 = "a".repeat(63);
        let sp_ascii_63_e = format!("{sp_ascii_63}é");
        let mut shadow_63_st = PgLiveSessionState::new(true, Some(true));
        shadow_63_st.server_encoding = Some(crate::sql::PostgresServerEncoding::from_pg_name("LATIN1"));
        shadow_63_st.apply_success("BEGIN", true, true);
        shadow_63_st.apply_success(&format!("SAVEPOINT {sp_ascii_63}"), true, true);
        shadow_63_st.apply_success("SET LOCAL standard_conforming_strings = off", true, false);
        shadow_63_st.apply_success(&format!("SAVEPOINT {sp_ascii_63_e}"), false, false);
        shadow_63_st.apply_success("SET LOCAL standard_conforming_strings = on", false, true);
        shadow_63_st.apply_success(&format!("ROLLBACK TO SAVEPOINT {sp_ascii_63}"), true, false);
        assert_eq!(
            shadow_63_st
                .savepoints
                .last()
                .and_then(|sp| sp.active_standard_conforming_strings),
            Some(false)
        );
        assert_eq!(shadow_63_st.session_scs, Some(true));
        let mut shadow_63_tracker = PgScsTracker::from_mode(
            &PgLiveSessionState {
                server_encoding: Some(crate::sql::PostgresServerEncoding::from_pg_name("LATIN1")),
                ..PgLiveSessionState::new(true, Some(true))
            }
            .to_session_mode(true),
        );
        shadow_63_tracker.apply("BEGIN");
        shadow_63_tracker.apply(&format!("SAVEPOINT {sp_ascii_63}"));
        shadow_63_tracker.apply("SET LOCAL standard_conforming_strings = off");
        shadow_63_tracker.apply(&format!("SAVEPOINT {sp_ascii_63_e}"));
        shadow_63_tracker.apply("SET LOCAL standard_conforming_strings = on");
        shadow_63_tracker.apply(&format!("ROLLBACK TO SAVEPOINT {sp_ascii_63}"));
        assert_eq!(shadow_63_tracker.active, Some(false));

        // Untracked non-ASCII savepoint after tracked ASCII sp_pre clears sp_pre so lookup fails closed,
        // whereas sp_post created *after* the untracked savepoint remains safely restorable.
        let mut untracked_shadow_st = PgLiveSessionState::new(true, Some(true));
        untracked_shadow_st.server_encoding =
            Some(crate::sql::PostgresServerEncoding::from_pg_name("LATIN1"));
        untracked_shadow_st.apply_success("BEGIN", true, true);
        untracked_shadow_st.apply_success("SAVEPOINT sp_pre", true, true);
        untracked_shadow_st.apply_success("SET LOCAL standard_conforming_strings = off", true, false);
        untracked_shadow_st.apply_success("SAVEPOINT aÉ", false, false);
        assert!(untracked_shadow_st.has_untracked_savepoints);
        assert!(untracked_shadow_st.savepoints.is_empty());
        let mut untracked_pre_lookup = untracked_shadow_st.clone();
        untracked_pre_lookup.apply_success("ROLLBACK TO SAVEPOINT sp_pre", false, true);
        assert_eq!(untracked_pre_lookup.session_scs, None);

        let mut untracked_tracker = PgScsTracker::from_mode(&untracked_shadow_st.to_session_mode(false));
        untracked_tracker.apply("ROLLBACK TO SAVEPOINT sp_pre");
        assert_eq!(untracked_tracker.active, None);

        untracked_shadow_st.apply_success("SAVEPOINT sp_post", false, false);
        untracked_shadow_st.apply_success("SET LOCAL standard_conforming_strings = on", false, true);
        let mut post_tracker = PgScsTracker::from_mode(&untracked_shadow_st.to_session_mode(true));
        post_tracker.apply("ROLLBACK TO SAVEPOINT sp_post");
        assert_eq!(post_tracker.active, Some(false));

        let mut other_enc_st = PgLiveSessionState::new(true, Some(true));
        other_enc_st.server_encoding = Some(crate::sql::PostgresServerEncoding::from_pg_name("SQL_ASCII"));
        other_enc_st.apply_success("BEGIN", true, true);
        other_enc_st.apply_success(&format!("SAVEPOINT {sp_lat1}"), true, true);
        assert!(other_enc_st.has_untracked_savepoints);
        let mut other_tracker = PgScsTracker::from_mode(&other_enc_st.to_session_mode(true));
        other_tracker.apply(&format!("ROLLBACK TO SAVEPOINT {sp_lat1}"));
        assert_eq!(other_tracker.active, None);

        let mut do_st = PgLiveSessionState::new(true, Some(true));
        do_st.apply_success("DO $$ BEGIN PERFORM 1; END $$", true, true);
        assert_eq!(do_st.tx_state, Some(PostgresTxState::Idle));
        do_st.apply_success("DO $$ BEGIN SET standard_conforming_strings = off; END $$", true, false);
        assert_eq!(do_st.tx_state, None);

        // 9. Auxiliary query errors (grid filters, metadata, FK lookups) abort active transactions
        //    and preserve pre_tx_scs so a follow-up COMMIT restores the pre-transaction string mode.
        let mut aux_st = PgLiveSessionState::new(true, Some(true));
        aux_st.apply_success("BEGIN", true, true);
        aux_st.apply_success("SET standard_conforming_strings = off", true, false);
        assert_eq!(aux_st.tx_state, Some(PostgresTxState::InTransaction));
        aux_st.apply_aux_query_error(Some("22012"));
        assert_eq!(aux_st.tx_state, Some(PostgresTxState::Aborted));
        assert_eq!(aux_st.pre_tx_scs, Some(true));
        aux_st.apply_success("COMMIT", false, true);
        assert_eq!(aux_st.tx_state, Some(PostgresTxState::Idle));
        assert_eq!(aux_st.session_scs, Some(true));

        let mut idle_aux_st = PgLiveSessionState::new(true, Some(true));
        idle_aux_st.apply_aux_query_error(Some("22012"));
        assert_eq!(idle_aux_st.tx_state, Some(PostgresTxState::Idle));

        let mut unknown_tx_aux = PgLiveSessionState::new(true, Some(true));
        unknown_tx_aux.tx_state = None;
        unknown_tx_aux.apply_aux_query_error(Some("25P02"));
        assert_eq!(unknown_tx_aux.tx_state, Some(PostgresTxState::Aborted));

        let mut ignore_disconnect_and_client = PgLiveSessionState::new(true, Some(true));
        ignore_disconnect_and_client.apply_success("BEGIN", true, true);
        ignore_disconnect_and_client.apply_aux_query_error(Some("57P01"));
        assert_eq!(ignore_disconnect_and_client.tx_state, Some(PostgresTxState::InTransaction));
        ignore_disconnect_and_client.apply_aux_query_error(None);
        assert_eq!(ignore_disconnect_and_client.tx_state, Some(PostgresTxState::InTransaction));

        // 10. SELECT set_config(...) with uncertain SCS effect preserves tx_state, pre_tx_scs, and savepoints.
        let mut uncertain_tx_st = PgLiveSessionState::new(true, Some(true));
        uncertain_tx_st.apply_success("BEGIN", true, true);
        uncertain_tx_st.apply_success("SAVEPOINT sp1", true, true);
        uncertain_tx_st.apply_success("SET LOCAL standard_conforming_strings = off", true, false);
        uncertain_tx_st.apply_success("SELECT set_config('work_mem'::text, '4MB', false)", false, false);
        assert_eq!(uncertain_tx_st.tx_state, Some(PostgresTxState::InTransaction));
        assert_eq!(uncertain_tx_st.pre_tx_scs, Some(true));
        assert_eq!(uncertain_tx_st.session_scs, None);
        assert_eq!(uncertain_tx_st.savepoints.len(), 1);
        assert_eq!(uncertain_tx_st.savepoints[0].name, "sp1");
        assert!(!uncertain_tx_st.has_untracked_savepoints);

        let uncertain_mode = uncertain_tx_st.to_session_mode(false);
        let mut rollback_tracker = PgScsTracker::from_mode(&uncertain_mode);
        rollback_tracker.apply("ROLLBACK");
        assert_eq!(rollback_tracker.tx_state, Some(PostgresTxState::Idle));
        assert_eq!(rollback_tracker.active, Some(true));

        let mut sp_tracker = PgScsTracker::from_mode(&uncertain_mode);
        sp_tracker.apply("ROLLBACK TO SAVEPOINT sp1");
        assert_eq!(sp_tracker.tx_state, Some(PostgresTxState::InTransaction));
        assert_eq!(sp_tracker.active, Some(true));

        let mut single_batch_tracker =
            PgScsTracker::from_mode(&PgLiveSessionState::new(true, Some(true)).to_session_mode(true));
        single_batch_tracker.apply("BEGIN");
        single_batch_tracker.apply("SAVEPOINT sp1");
        single_batch_tracker.apply("SET LOCAL standard_conforming_strings = off");
        single_batch_tracker.apply("SELECT set_config('work_mem'::text, '4MB', false)");
        assert_eq!(single_batch_tracker.tx_state, Some(PostgresTxState::InTransaction));
        assert_eq!(single_batch_tracker.active, None);
        single_batch_tracker.apply("ROLLBACK");
        assert_eq!(single_batch_tracker.tx_state, Some(PostgresTxState::Idle));
        assert_eq!(single_batch_tracker.active, Some(true));
        assert!(evaluate_mid_batch_pg_mode(true, false, single_batch_tracker.active, "SELECT 1").is_ok());

        let mut multi_stmt_tracker = PgScsTracker::from_mode(&uncertain_mode);
        multi_stmt_tracker.apply("SELECT set_config('work_mem'::text, '4MB', false); ROLLBACK");
        assert_eq!(multi_stmt_tracker.tx_state, None);
        assert_eq!(multi_stmt_tracker.active, None);
        assert_eq!(
            multi_stmt_tracker.to_session_mode().pre_tx_standard_conforming_strings,
            None
        );
        assert!(multi_stmt_tracker.to_session_mode().savepoints.is_empty());
        assert!(multi_stmt_tracker.to_session_mode().has_untracked_savepoints);

        let mut idle_uncertain_st = PgLiveSessionState::new(true, Some(true));
        idle_uncertain_st.apply_success(
            "SELECT set_config('standard_conforming_strings'::text, 'off', false)",
            true,
            false,
        );
        assert_eq!(idle_uncertain_st.tx_state, Some(PostgresTxState::Idle));
        assert_eq!(idle_uncertain_st.session_scs, Some(false));
        assert_eq!(idle_uncertain_st.pre_tx_scs, Some(false));

        let mut idle_batch_tracker =
            PgScsTracker::from_mode(&PgLiveSessionState::new(true, Some(true)).to_session_mode(true));
        idle_batch_tracker.apply("SELECT set_config('standard_conforming_strings'::text, 'off', false)");
        assert_eq!(idle_batch_tracker.tx_state, Some(PostgresTxState::Idle));
        assert_eq!(idle_batch_tracker.active, None);
        idle_batch_tracker.apply("BEGIN");
        idle_batch_tracker.apply("ROLLBACK");
        assert_eq!(idle_batch_tracker.tx_state, Some(PostgresTxState::Idle));
        assert_eq!(idle_batch_tracker.active, None);
    }
}
