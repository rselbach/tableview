use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use mysql_async::consts::{ColumnFlags, ColumnType};
use mysql_async::prelude::Queryable;
use mysql_async::{
    ClientIdentity, Column, DriverError, Error as MysqlError, IoError, Opts, OptsBuilder, Params, Row, SslOpts, Value,
};
use uuid::Uuid;

use crate::connection::{ConnectionProfile, DbEngine, EnvTag, PasswordMode, SafetyPolicy, SslConfig, SslMode};
use crate::engine::{
    CellValue, ColumnMeta, ColumnSummary, DatabaseInfo, DbError, DbObject, DbObjectKind, DbSession, DecodedPageBatch,
    ExecutedSql, FkLookupComponent, FkPeekRow, ForeignKeyRef, IndexInfo, MysqlRawValue, PageRequest, QueryBatchResult,
    QueryStatementResult, RawCellValue, SchemaInfo, ServerInfo, StableOrderKey, TableMetaWithOrder, TablePage, TableStats,
    TableStructure, TableTransactionSupport, TransactionOutcome, attach_foreign_keys_to_columns, decode_hex,
    last_page_offset,
};
use crate::explain::{ExplainPlan, build_explain_sql_for_engine};
use crate::staging::{
    LogicalColType, StagedCellValue, StagedColumnValue, StagedCommitBatch, StagedMutation,
    build_fk_where_clause_for_engine, classify_column_type, format_fk_lookup_display, is_mysql_text_col,
    strip_hex_prefix,
};

static CRYPTO_PROVIDER_INIT: OnceLock<()> = OnceLock::new();
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const PING_TIMEOUT: Duration = Duration::from_secs(3);
const MYSQL_POOL_MAX: usize = 4;

fn ensure_crypto_provider() {
    CRYPTO_PROVIDER_INIT.get_or_init(|| {
        drop(rustls::crypto::ring::default_provider().install_default());
    });
}

struct ActiveMysqlQuery {
    conn_id: u64,
    cancel_requested: Arc<AtomicBool>,
}

struct IdlePool<T> {
    conns: VecDeque<T>,
    generation: u64,
}

impl<T> IdlePool<T> {
    fn new(initial: VecDeque<T>) -> Self {
        Self {
            conns: initial,
            generation: 0,
        }
    }

    fn generation(&self) -> u64 {
        self.generation
    }

    fn pop(&mut self) -> (Option<T>, u64) {
        (self.conns.pop_back(), self.generation)
    }

    fn try_return(&mut self, conn: T, expected_gen: u64, max_idle: usize) -> Result<(), T> {
        if self.generation == expected_gen && self.conns.len() < max_idle {
            self.conns.push_back(conn);
            Ok(())
        } else {
            Err(conn)
        }
    }

    fn evict(&mut self) -> Vec<T> {
        self.generation = self.generation.wrapping_add(1);
        self.conns.drain(..).collect()
    }
}

struct PooledMysqlConn {
    inner: Option<(mysql_async::Conn, tokio::sync::OwnedSemaphorePermit)>,
    idle_conns: Arc<std::sync::Mutex<IdlePool<mysql_async::Conn>>>,
    checkout_gen: u64,
}

impl PooledMysqlConn {
    fn conn_mut(&mut self) -> Result<&mut mysql_async::Conn, DbError> {
        self.inner
            .as_mut()
            .map(|(conn, _)| conn)
            .ok_or(DbError::Disconnected)
    }

    fn return_clean(mut self) {
        let Some((conn, permit)) = self.inner.take() else {
            return;
        };
        let excess = match self.idle_conns.lock() {
            Ok(mut pool) => pool.try_return(conn, self.checkout_gen, MYSQL_POOL_MAX).err(),
            Err(_) => Some(conn),
        };
        if let Some(conn) = excess
            && let Ok(handle) = tokio::runtime::Handle::try_current()
        {
            handle.spawn(async move {
                drop(tokio::time::timeout(Duration::from_millis(500), conn.disconnect()).await);
                drop(permit);
            });
            return;
        }
        drop(permit);
    }

    fn discard(mut self) {
        let Some((conn, permit)) = self.inner.take() else {
            return;
        };
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                drop(tokio::time::timeout(Duration::from_millis(500), conn.disconnect()).await);
                drop(permit);
            });
        }
    }
}

impl Drop for PooledMysqlConn {
    fn drop(&mut self) {
        let Some((mut conn, permit)) = self.inner.take() else {
            return;
        };
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let idle_conns = Arc::clone(&self.idle_conns);
            handle.spawn(async move {
                let reset_gen = idle_conns.lock().ok().map(|pool| pool.generation());
                let reset_ok = matches!(
                    tokio::time::timeout(PING_TIMEOUT, conn.reset()).await,
                    Ok(Ok(true))
                );
                if reset_ok {
                    let excess = if let Some(expected_gen) = reset_gen {
                        match idle_conns.lock() {
                            Ok(mut pool) => pool.try_return(conn, expected_gen, MYSQL_POOL_MAX).err(),
                            Err(_) => Some(conn),
                        }
                    } else {
                        Some(conn)
                    };
                    if let Some(conn) = excess {
                        drop(tokio::time::timeout(Duration::from_millis(500), conn.disconnect()).await);
                    }
                    drop(permit);
                    return;
                }
                drop(tokio::time::timeout(Duration::from_millis(500), conn.disconnect()).await);
                drop(permit);
            });
        }
    }
}

async fn acquire_pool_slot(
    slots: &Arc<tokio::sync::Semaphore>,
    timeout: Duration,
) -> Result<tokio::sync::OwnedSemaphorePermit, DbError> {
    match tokio::time::timeout(timeout, Arc::clone(slots).acquire_owned()).await {
        Ok(Ok(permit)) => Ok(permit),
        Ok(Err(_)) => Err(DbError::Disconnected),
        Err(_) => Err(DbError::ConnectionFailed(
            "all pooled connections are busy; wait for a running query to finish and retry".to_owned(),
        )),
    }
}

const MYSQL_QUOTE_MODE_UNKNOWN: u64 = u64::MAX;
const MYSQL_LIVE_SESSION_MODE_SQL: &str = "SELECT VERSION(), COALESCE(@@SESSION.sql_mode, '')";
const MYSQL_LIVE_GLOBAL_MODE_SQL: &str = "SELECT VERSION(), COALESCE(@@GLOBAL.sql_mode, '')";

fn pack_mysql_quote_mode(mode: Option<crate::sql::MysqlQuoteMode>) -> u64 {
    let Some(mode) = mode else {
        return MYSQL_QUOTE_MODE_UNKNOWN;
    };
    let mut bits = u64::from(mode.to_bits() & 0b111);
    if let Some(sv) = mode.server_version {
        bits |= 1 << 3;
        if matches!(sv.flavor, crate::sql::MysqlFlavor::MariaDb) {
            bits |= 1 << 4;
        }
        bits |= u64::from(sv.version_id) << 5;
    }
    bits
}

fn unpack_mysql_quote_mode(bits: u64) -> Option<crate::sql::MysqlQuoteMode> {
    if bits == MYSQL_QUOTE_MODE_UNKNOWN {
        return None;
    }
    let base = crate::sql::MysqlQuoteMode::from_bits((bits & 0b111) as u8);
    let server_version = if (bits & (1 << 3)) != 0 {
        let flavor = if (bits & (1 << 4)) != 0 {
            crate::sql::MysqlFlavor::MariaDb
        } else {
            crate::sql::MysqlFlavor::Mysql
        };
        Some(crate::sql::MysqlServerVersion {
            flavor,
            version_id: ((bits >> 5) & 0xFFFF_FFFF) as u32,
        })
    } else {
        None
    };
    Some(base.with_server_version(server_version))
}

fn parse_mysql_live_mode_row(row: &Row) -> crate::sql::MysqlQuoteMode {
    let raw_version = value_to_string(row.as_ref(0).unwrap_or(&Value::NULL));
    let raw_sql_mode = value_to_string(row.as_ref(1).unwrap_or(&Value::NULL));
    let server_version = crate::sql::MysqlServerVersion::from_version_str(&raw_version);
    crate::sql::MysqlQuoteMode::from_sql_mode(&raw_sql_mode).with_server_version(server_version)
}

fn evaluate_live_mysql_mode(
    live_mode: crate::sql::MysqlQuoteMode,
    expected_mode: Option<crate::sql::MysqlQuoteMode>,
) -> Result<(), DbError> {
    if Some(live_mode) == expected_mode {
        Ok(())
    } else {
        Err(DbError::MysqlModeMismatch)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum CheckedConnProbeAction {
    Ready,
    DiscardAndRetry,
    DiscardAndRetryAfterTimeout,
    DiscardAndDisconnect,
    DiscardAndFail(DbError),
}

fn classify_mode_refresh_probe(
    res: Result<Result<bool, DbError>, ()>,
    retry_once_on_timeout: bool,
    timed_out_once: bool,
) -> CheckedConnProbeAction {
    match res {
        Ok(Ok(true)) => CheckedConnProbeAction::Ready,
        Ok(Ok(false)) => CheckedConnProbeAction::DiscardAndRetry,
        Ok(Err(err)) => {
            if matches!(err, DbError::Disconnected) {
                CheckedConnProbeAction::DiscardAndRetry
            } else {
                CheckedConnProbeAction::DiscardAndFail(err)
            }
        }
        Err(()) => {
            if retry_once_on_timeout && !timed_out_once {
                CheckedConnProbeAction::DiscardAndRetryAfterTimeout
            } else {
                CheckedConnProbeAction::DiscardAndDisconnect
            }
        }
    }
}

/// Active MySQL / MariaDB database session backed by `mysql_async`.
pub struct MysqlSession {
    opts: Opts,
    server_info: ServerInfo,
    read_only: bool,
    mysql_quote_mode: AtomicU64,
    mysql_grid_quote_mode: AtomicU64,
    active_conn_ids: Arc<std::sync::Mutex<HashMap<u64, ActiveMysqlQuery>>>,
    idle_conns: Arc<std::sync::Mutex<IdlePool<mysql_async::Conn>>>,
    pool_slots: Arc<tokio::sync::Semaphore>,
    editor_conn: tokio::sync::Mutex<Option<mysql_async::Conn>>,
    disconnected: AtomicBool,
    tx_lock: tokio::sync::Mutex<()>,
}

fn spawn_disconnect_mysql_conn(conn: mysql_async::Conn) {
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move {
            drop(tokio::time::timeout(Duration::from_millis(500), conn.disconnect()).await);
        });
    }
}

impl Drop for MysqlSession {
    fn drop(&mut self) {
        self.evict_idle_conns();
        if let Ok(mut guard) = self.editor_conn.try_lock()
            && let Some(conn) = guard.take()
        {
            spawn_disconnect_mysql_conn(conn);
        }
    }
}

struct ActiveConnMapGuard<'a> {
    map: &'a std::sync::Mutex<HashMap<u64, ActiveMysqlQuery>>,
    cancel_key: u64,
}

impl Drop for ActiveConnMapGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.map.lock() {
            guard.remove(&self.cancel_key);
        }
    }
}

impl MysqlSession {
    /// Connects to MySQL/MariaDB using the supplied profile, optional password, and optional database override.
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

        let (mut conn, opts) = connect_conn_with_fallback(profile, password, target_db).await?;

        let info_sql =
            "SELECT VERSION(), COALESCE(DATABASE(), ''), CURRENT_USER(), COALESCE(@@SESSION.sql_mode, '')";
        let start = Instant::now();
        let row: Option<Row> = match conn.query_first(info_sql).await {
            Ok(row) => row,
            Err(err) => {
                let mapped = map_mysql_err(err);
                drop(tokio::time::timeout(Duration::from_millis(500), conn.disconnect()).await);
                return Err(mapped);
            }
        };
        let duration_ms = start.elapsed().as_millis() as u64;

        let Some(row) = row else {
            drop(tokio::time::timeout(Duration::from_millis(500), conn.disconnect()).await);
            return Err(DbError::ConnectionFailed(
                "MySQL server returned no rows for version query".to_owned(),
            ));
        };

        let raw_version = value_to_string(row.as_ref(0).unwrap_or(&Value::NULL));
        let current_database = value_to_string(row.as_ref(1).unwrap_or(&Value::NULL));
        let current_user = value_to_string(row.as_ref(2).unwrap_or(&Value::NULL));
        let raw_sql_mode = value_to_string(row.as_ref(3).unwrap_or(&Value::NULL));
        let server_version = crate::sql::MysqlServerVersion::from_version_str(&raw_version);
        let quote_mode =
            crate::sql::MysqlQuoteMode::from_sql_mode(&raw_sql_mode).with_server_version(server_version);

        let server_info = ServerInfo {
            version: concise_mysql_version(&raw_version),
            current_database,
            current_user,
        };
        let log = ExecutedSql {
            sql: info_sql.to_owned(),
            duration_ms,
            error: None,
        };

        Ok((
            Self {
                opts,
                server_info,
                read_only: profile.safety == SafetyPolicy::ReadOnly,
                mysql_quote_mode: AtomicU64::new(pack_mysql_quote_mode(Some(quote_mode))),
                mysql_grid_quote_mode: AtomicU64::new(pack_mysql_quote_mode(Some(quote_mode))),
                active_conn_ids: Arc::new(std::sync::Mutex::new(HashMap::new())),
                idle_conns: Arc::new(std::sync::Mutex::new(IdlePool::new(VecDeque::from([conn])))),
                pool_slots: Arc::new(tokio::sync::Semaphore::new(MYSQL_POOL_MAX)),
                editor_conn: tokio::sync::Mutex::new(None),
                disconnected: AtomicBool::new(false),
                tx_lock: tokio::sync::Mutex::new(()),
            },
            vec![log],
        ))
    }

    /// Tests connectivity and returns concise server metadata without retaining an open pool.
    pub async fn test_connection(
        profile: &ConnectionProfile,
        password: Option<&str>,
    ) -> Result<ServerInfo, DbError> {
        let (session, _) = Self::connect(profile, password, None).await?;
        let info = session.server_info().clone();
        let conns: Vec<_> = session
            .idle_conns
            .lock()
            .map(|mut p| p.evict())
            .unwrap_or_default();
        for conn in conns {
            drop(tokio::time::timeout(Duration::from_millis(500), conn.disconnect()).await);
        }
        Ok(info)
    }

    fn evict_idle_conns(&self) {
        let Ok(mut pool) = self.idle_conns.lock() else {
            return;
        };
        let conns = pool.evict();
        drop(pool);
        if !conns.is_empty()
            && let Ok(handle) = tokio::runtime::Handle::try_current()
        {
            handle.spawn(async move {
                for conn in conns {
                    drop(tokio::time::timeout(Duration::from_millis(500), conn.disconnect()).await);
                }
            });
        }
    }

    fn ensure_alive(&self) -> Result<(), DbError> {
        if self.is_disconnected() {
            return Err(DbError::Disconnected);
        }
        Ok(())
    }

    fn map_session_err(&self, err: MysqlError) -> DbError {
        let mapped = map_mysql_session_err(err);
        if matches!(mapped, DbError::Disconnected) {
            self.disconnected.store(true, Ordering::Release);
        }
        mapped
    }

    fn map_checkout_err(&self, err: MysqlError) -> DbError {
        let mapped = map_mysql_session_err(err);
        if matches!(
            mapped,
            DbError::Disconnected | DbError::AuthFailed(_) | DbError::ConnectionFailed(_) | DbError::TlsConfig(_)
        ) {
            self.disconnected.store(true, Ordering::Release);
        }
        mapped
    }

    async fn acquire_editor_conn(
        &self,
        guard: &mut Option<mysql_async::Conn>,
        cancel_flag: Option<&AtomicBool>,
    ) -> Result<mysql_async::Conn, DbError> {
        let is_cancelled = || cancel_flag.is_some_and(|f| f.load(Ordering::Acquire));
        if is_cancelled() {
            return Err(DbError::Cancelled);
        }
        if let Err(err) = self.ensure_alive() {
            if let Some(stale) = guard.take() {
                spawn_disconnect_mysql_conn(stale);
            }
            return Err(DbError::DisconnectedBeforeDispatch(Box::new(err)));
        }
        if let Some(mut conn) = guard.take() {
            match tokio::time::timeout(PING_TIMEOUT, conn.ping()).await {
                Ok(Ok(())) => {
                    if is_cancelled() {
                        *guard = Some(conn);
                        return Err(DbError::Cancelled);
                    }
                    return Ok(conn);
                }
                Ok(Err(err)) => {
                    spawn_disconnect_mysql_conn(conn);
                    self.disconnected.store(true, Ordering::Release);
                    self.mysql_quote_mode.store(MYSQL_QUOTE_MODE_UNKNOWN, Ordering::Release);
                    let mapped = map_mysql_session_err(err);
                    let cause = if matches!(mapped, DbError::Disconnected) {
                        mapped
                    } else {
                        DbError::Disconnected
                    };
                    return Err(DbError::DisconnectedBeforeDispatch(Box::new(cause)));
                }
                Err(_) => {
                    spawn_disconnect_mysql_conn(conn);
                    self.disconnected.store(true, Ordering::Release);
                    self.mysql_quote_mode.store(MYSQL_QUOTE_MODE_UNKNOWN, Ordering::Release);
                    return Err(DbError::DisconnectedBeforeDispatch(Box::new(DbError::Disconnected)));
                }
            }
        }

        let mut conn = match tokio::time::timeout(CONNECT_TIMEOUT, mysql_async::Conn::new(self.opts.clone())).await {
            Ok(Ok(conn)) => conn,
            Ok(Err(err)) => {
                let mapped = self.map_checkout_err(err);
                if mapped.is_auth_failure() {
                    return Err(mapped);
                }
                return Err(DbError::DisconnectedBeforeDispatch(Box::new(mapped)));
            }
            Err(_) => {
                self.disconnected.store(true, Ordering::Release);
                return Err(DbError::DisconnectedBeforeDispatch(Box::new(
                    DbError::ConnectionFailed("connection timed out".to_owned()),
                )));
            }
        };

        match tokio::time::timeout(
            PING_TIMEOUT,
            conn.query_first::<Row, _>(MYSQL_LIVE_SESSION_MODE_SQL),
        )
        .await
        {
            Ok(Ok(Some(row))) => {
                let mode = parse_mysql_live_mode_row(&row);
                self.mysql_quote_mode
                    .store(pack_mysql_quote_mode(Some(mode)), Ordering::Release);
            }
            Ok(Ok(None)) | Err(_) => {
                spawn_disconnect_mysql_conn(conn);
                self.disconnected.store(true, Ordering::Release);
                self.mysql_quote_mode.store(MYSQL_QUOTE_MODE_UNKNOWN, Ordering::Release);
                return Err(DbError::DisconnectedBeforeDispatch(Box::new(DbError::Disconnected)));
            }
            Ok(Err(err)) => {
                spawn_disconnect_mysql_conn(conn);
                let mapped = self.map_checkout_err(err);
                if mapped.is_auth_failure() {
                    return Err(mapped);
                }
                return Err(DbError::DisconnectedBeforeDispatch(Box::new(mapped)));
            }
        }

        if is_cancelled() {
            *guard = Some(conn);
            return Err(DbError::Cancelled);
        }
        Ok(conn)
    }

    async fn verify_editor_mysql_mode(
        &self,
        conn: &mut mysql_async::Conn,
        expected_mode: Option<crate::sql::MysqlQuoteMode>,
        cancel_flag: Option<&AtomicBool>,
    ) -> Result<crate::sql::MysqlQuoteMode, DbError> {
        let is_cancelled = || cancel_flag.is_some_and(|f| f.load(Ordering::Acquire));
        if is_cancelled() {
            return Err(DbError::Cancelled);
        }
        match tokio::time::timeout(
            PING_TIMEOUT,
            conn.query_first::<Row, _>(MYSQL_LIVE_SESSION_MODE_SQL),
        )
        .await
        {
            Ok(Ok(Some(row))) => {
                let live_mode = parse_mysql_live_mode_row(&row);
                self.mysql_quote_mode
                    .store(pack_mysql_quote_mode(Some(live_mode)), Ordering::Release);
                if is_cancelled() {
                    return Err(DbError::Cancelled);
                }
                if evaluate_live_mysql_mode(live_mode, expected_mode).is_err() {
                    return Err(DbError::MysqlModeMismatch);
                }
                Ok(live_mode)
            }
            Ok(Ok(None)) | Err(_) => {
                self.disconnected.store(true, Ordering::Release);
                Err(DbError::DisconnectedBeforeDispatch(Box::new(DbError::Disconnected)))
            }
            Ok(Err(err)) => {
                let mapped = self.map_session_err(err);
                if matches!(mapped, DbError::Disconnected) {
                    Err(DbError::DisconnectedBeforeDispatch(Box::new(mapped)))
                } else {
                    Err(mapped)
                }
            }
        }
    }

    async fn get_conn(&self) -> Result<PooledMysqlConn, DbError> {
        self.ensure_alive()?;
        let permit = acquire_pool_slot(&self.pool_slots, CONNECT_TIMEOUT).await?;
        let (idle_conn, checkout_gen) = self
            .idle_conns
            .lock()
            .map_err(|_| DbError::Disconnected)?
            .pop();
        if let Some(conn) = idle_conn {
            return Ok(PooledMysqlConn {
                inner: Some((conn, permit)),
                idle_conns: Arc::clone(&self.idle_conns),
                checkout_gen,
            });
        }
        match tokio::time::timeout(CONNECT_TIMEOUT, mysql_async::Conn::new(self.opts.clone())).await {
            Ok(Ok(conn)) => Ok(PooledMysqlConn {
                inner: Some((conn, permit)),
                idle_conns: Arc::clone(&self.idle_conns),
                checkout_gen,
            }),
            Ok(Err(err)) => Err(self.map_checkout_err(err)),
            Err(_) => {
                self.disconnected.store(true, Ordering::Release);
                Err(DbError::ConnectionFailed("connection timed out".to_owned()))
            }
        }
    }

    async fn get_checked_conn(
        &self,
        retry_once_on_timeout: bool,
        wrap_pre_dispatch: bool,
        cancel_flag: Option<&AtomicBool>,
    ) -> Result<PooledMysqlConn, DbError> {
        let wrap_err = |err: DbError| -> DbError {
            if wrap_pre_dispatch {
                DbError::DisconnectedBeforeDispatch(Box::new(err))
            } else {
                err
            }
        };
        let is_cancelled = || cancel_flag.is_some_and(|f| f.load(Ordering::Acquire));

        if is_cancelled() {
            return Err(DbError::Cancelled);
        }
        if let Err(err) = self.ensure_alive() {
            return Err(wrap_err(err));
        }

        let mut timed_out_once = false;
        for _ in 0..=MYSQL_POOL_MAX {
            if is_cancelled() {
                return Err(DbError::Cancelled);
            }
            let mut lease = match self.get_conn().await {
                Ok(c) => c,
                Err(err) => {
                    return if self.is_disconnected() {
                        Err(wrap_err(err))
                    } else {
                        Err(err)
                    };
                }
            };
            let ping_res = {
                let conn = lease.conn_mut()?;
                tokio::time::timeout(PING_TIMEOUT, conn.ping()).await
            };
            match ping_res {
                Ok(Ok(())) => {
                    if is_cancelled() {
                        lease.return_clean();
                        return Err(DbError::Cancelled);
                    }
                    if self.mysql_grid_quote_mode.load(Ordering::Acquire) == MYSQL_QUOTE_MODE_UNKNOWN {
                        let mode_res = {
                            let conn = lease.conn_mut()?;
                            tokio::time::timeout(
                                PING_TIMEOUT,
                                conn.query_first::<Row, _>(MYSQL_LIVE_GLOBAL_MODE_SQL),
                            )
                            .await
                        };
                        let mut fetched_row: Option<Row> = None;
                        let mapped_res: Result<Result<bool, DbError>, ()> = match mode_res {
                            Ok(Ok(Some(row))) => {
                                fetched_row = Some(row);
                                Ok(Ok(true))
                            }
                            Ok(Ok(None)) => Ok(Ok(false)),
                            Ok(Err(err)) => Ok(Err(map_mysql_session_err(err))),
                            Err(_) => Err(()),
                        };
                        match classify_mode_refresh_probe(
                            mapped_res,
                            retry_once_on_timeout,
                            timed_out_once,
                        ) {
                            CheckedConnProbeAction::Ready => {
                                if let Some(mode_row) = fetched_row {
                                    let refreshed = parse_mysql_live_mode_row(&mode_row);
                                    self.mysql_grid_quote_mode.store(
                                        pack_mysql_quote_mode(Some(refreshed)),
                                        Ordering::Release,
                                    );
                                }
                                if is_cancelled() {
                                    lease.return_clean();
                                    return Err(DbError::Cancelled);
                                }
                            }
                            CheckedConnProbeAction::DiscardAndRetry => {
                                lease.discard();
                                continue;
                            }
                            CheckedConnProbeAction::DiscardAndRetryAfterTimeout => {
                                lease.discard();
                                timed_out_once = true;
                                continue;
                            }
                            CheckedConnProbeAction::DiscardAndDisconnect => {
                                lease.discard();
                                self.disconnected.store(true, Ordering::Release);
                                return Err(wrap_err(DbError::Disconnected));
                            }
                            CheckedConnProbeAction::DiscardAndFail(err) => {
                                lease.discard();
                                return Err(err);
                            }
                        }
                    }
                    return Ok(lease);
                }
                Ok(Err(err)) => {
                    lease.discard();
                    let mapped = map_mysql_session_err(err);
                    if !matches!(mapped, DbError::Disconnected) {
                        return Err(mapped);
                    }
                }
                Err(_) => {
                    lease.discard();
                    if retry_once_on_timeout && !timed_out_once {
                        timed_out_once = true;
                        continue;
                    }
                    self.disconnected.store(true, Ordering::Release);
                    return Err(wrap_err(DbError::Disconnected));
                }
            }
        }
        self.disconnected.store(true, Ordering::Release);
        Err(wrap_err(DbError::Disconnected))
    }

    async fn fetch_column_metas(
        &self,
        schema: &str,
        table: &str,
    ) -> Result<(Vec<ColumnMeta>, Vec<ForeignKeyRef>, ExecutedSql), DbError> {
        let meta = self.fetch_column_metas_and_order(schema, table).await?;
        Ok((meta.columns, meta.foreign_keys, meta.log))
    }

    async fn fetch_column_metas_and_order(
        &self,
        schema: &str,
        table: &str,
    ) -> Result<TableMetaWithOrder, DbError> {
        let mut lease = self.get_conn().await?;
        let conn = lease.conn_mut()?;
        let col_sql = "SELECT COLUMN_NAME, COLUMN_TYPE, DATA_TYPE, IS_NULLABLE, COLUMN_DEFAULT, \
                       COLUMN_KEY, COLUMN_COMMENT, EXTRA \
                       FROM information_schema.COLUMNS \
                       WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? \
                       ORDER BY ORDINAL_POSITION ASC";
        let start = Instant::now();
        let col_rows: Vec<Row> = conn.exec(col_sql, (schema, table)).await.map_err(|e| self.map_session_err(e))?;

        let fk_sql = "SELECT COLUMN_NAME, CONSTRAINT_NAME, REFERENCED_TABLE_SCHEMA, \
                      REFERENCED_TABLE_NAME, REFERENCED_COLUMN_NAME, ORDINAL_POSITION \
                      FROM information_schema.KEY_COLUMN_USAGE \
                      WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND REFERENCED_TABLE_NAME IS NOT NULL \
                      ORDER BY CONSTRAINT_NAME ASC, ORDINAL_POSITION ASC";
        let fk_rows: Vec<Row> = conn.exec(fk_sql, (schema, table)).await.map_err(|e| self.map_session_err(e))?;

        let stats_sql = "SELECT INDEX_NAME, NON_UNIQUE, COLUMN_NAME, SEQ_IN_INDEX, SUB_PART \
                         FROM information_schema.STATISTICS \
                         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? \
                         ORDER BY INDEX_NAME ASC, SEQ_IN_INDEX ASC";
        let stat_rows: Vec<Row> = conn.exec(stats_sql, (schema, table)).await.map_err(|e| self.map_session_err(e))?;
        drop(lease);

        let raw_fk_rows: Vec<MysqlFkRowTuple> = fk_rows
            .into_iter()
            .map(|row| {
                (
                    value_to_opt_string(row.as_ref(0).unwrap_or(&Value::NULL)),
                    value_to_string(row.as_ref(1).unwrap_or(&Value::NULL)),
                    value_to_opt_string(row.as_ref(2).unwrap_or(&Value::NULL)),
                    value_to_opt_string(row.as_ref(3).unwrap_or(&Value::NULL)),
                    value_to_opt_string(row.as_ref(4).unwrap_or(&Value::NULL)),
                    value_to_opt_i64(row.as_ref(5).unwrap_or(&Value::NULL)),
                )
            })
            .collect();
        let foreign_keys = assemble_mysql_foreign_keys(schema, raw_fk_rows);
        let mut fks = attach_foreign_keys_to_columns(foreign_keys.clone());

        let mut index_cols: HashMap<String, MysqlIndexMeta> = HashMap::new();
        for row in stat_rows {
            let index_name = value_to_string(row.as_ref(0).unwrap_or(&Value::NULL));
            let non_unique = value_to_opt_i64(row.as_ref(1).unwrap_or(&Value::NULL)).unwrap_or(1);
            let col_name = value_to_opt_string(row.as_ref(2).unwrap_or(&Value::NULL)).filter(|s| !s.is_empty());
            let is_prefix = value_to_opt_i64(row.as_ref(4).unwrap_or(&Value::NULL)).is_some();
            let entry = index_cols
                .entry(index_name)
                .or_insert_with(|| MysqlIndexMeta {
                    is_unique: non_unique == 0,
                    cols: Vec::new(),
                });
            entry.cols.push(MysqlIndexCol {
                name: col_name,
                is_prefix,
            });
        }

        let mut pk_cols = HashSet::new();
        let mut unique_cols = HashSet::new();
        for (idx_name, idx_meta) in &index_cols {
            if idx_name == "PRIMARY" {
                for col in &idx_meta.cols {
                    if let Some(c) = &col.name {
                        pk_cols.insert(c.clone());
                    }
                }
            } else if idx_meta.is_unique
                && idx_meta.cols.len() == 1
                && let Some(MysqlIndexCol {
                    name: Some(only_col),
                    is_prefix: false,
                }) = idx_meta.cols.first()
            {
                unique_cols.insert(only_col.clone());
            }
        }

        let columns: Vec<ColumnMeta> = col_rows
            .into_iter()
            .map(|row| {
                let name = value_to_string(row.as_ref(0).unwrap_or(&Value::NULL));
                let column_type = value_to_string(row.as_ref(1).unwrap_or(&Value::NULL));
                let data_type_raw = value_to_string(row.as_ref(2).unwrap_or(&Value::NULL));
                let is_nullable = value_to_string(row.as_ref(3).unwrap_or(&Value::NULL));
                let column_default = value_to_opt_string(row.as_ref(4).unwrap_or(&Value::NULL));
                let column_key = value_to_string(row.as_ref(5).unwrap_or(&Value::NULL));
                let comment = value_to_opt_string(row.as_ref(6).unwrap_or(&Value::NULL))
                    .filter(|s| !s.trim().is_empty());
                let extra = value_to_string(row.as_ref(7).unwrap_or(&Value::NULL));

                let default_expr = if column_default.is_some() {
                    column_default
                } else if extra.to_ascii_lowercase().contains("auto_increment") {
                    Some("auto_increment".to_owned())
                } else {
                    None
                };

                let is_primary_key = pk_cols.contains(&name) || column_key.eq_ignore_ascii_case("PRI");
                let is_unique = !is_primary_key
                    && (unique_cols.contains(&name) || column_key.eq_ignore_ascii_case("UNI"));
                let foreign_key = fks.remove(&name);

                ColumnMeta {
                    name,
                    data_type: column_type,
                    udt_name: data_type_raw.to_ascii_lowercase(),
                    nullable: is_nullable.eq_ignore_ascii_case("YES"),
                    default_expr,
                    is_primary_key,
                    is_unique,
                    comment,
                    foreign_key,
                }
            })
            .collect();

        let stable_order = select_mysql_stable_order_keys(&columns, &index_cols);
        let log = ExecutedSql {
            sql: format!(
                "{col_sql} -- ? = '{}', ? = '{}'",
                schema.replace('\'', "''"),
                table.replace('\'', "''")
            ),
            duration_ms: start.elapsed().as_millis() as u64,
            error: None,
        };
        Ok(TableMetaWithOrder {
            columns,
            foreign_keys,
            stable_order,
            log,
        })
    }

    async fn execute_mysql_page_query(
        &self,
        conn: &mut mysql_async::Conn,
        columns: &[ColumnMeta],
        req: &PageRequest,
        stable_order: &[StableOrderKey],
        limit: i64,
        offset: i64,
    ) -> Result<DecodedPageBatch, DbError> {
        let sql = build_mysql_page_sql(&req.schema, &req.table, columns, req, stable_order, limit + 1, offset);
        let start = Instant::now();
        let mysql_rows: Vec<Row> = conn.query(&sql).await.map_err(|e| self.map_session_err(e))?;
        let elapsed_ms = start.elapsed().as_millis() as u64;

        let has_more = mysql_rows.len() > limit as usize;
        let row_count = mysql_rows.len().min(limit as usize);
        let mut decoded_rows = Vec::with_capacity(row_count);
        let mut raw_rows = Vec::with_capacity(row_count);
        for row in mysql_rows.into_iter().take(limit as usize) {
            let wire_cols = row.columns_ref();
            let mut cells = Vec::with_capacity(columns.len());
            let mut raw_cells = Vec::with_capacity(columns.len());
            for (idx, col) in columns.iter().enumerate() {
                let val = row.as_ref(idx).unwrap_or(&Value::NULL);
                let wire_col = wire_cols.get(idx);
                raw_cells.push(RawCellValue::Mysql(mysql_val_to_raw(val)));
                cells.push(decode_mysql_cell(val, wire_col, Some(col)));
            }
            decoded_rows.push(cells);
            raw_rows.push(raw_cells);
        }

        let log = ExecutedSql {
            sql,
            duration_ms: elapsed_ms,
            error: None,
        };
        Ok(DecodedPageBatch {
            rows: decoded_rows,
            raw_rows,
            has_more,
            log,
        })
    }
}

#[async_trait]
impl DbSession for MysqlSession {
    fn engine(&self) -> DbEngine {
        DbEngine::Mysql
    }

    fn server_info(&self) -> &ServerInfo {
        &self.server_info
    }

    fn mysql_quote_mode(&self) -> Option<crate::sql::MysqlQuoteMode> {
        unpack_mysql_quote_mode(self.mysql_quote_mode.load(Ordering::Acquire))
    }

    fn mysql_grid_quote_mode(&self) -> Option<crate::sql::MysqlQuoteMode> {
        unpack_mysql_quote_mode(self.mysql_grid_quote_mode.load(Ordering::Acquire))
    }

    fn is_disconnected(&self) -> bool {
        self.disconnected.load(Ordering::Acquire)
    }

    async fn list_databases(&self) -> Result<(Vec<DatabaseInfo>, ExecutedSql), DbError> {
        let mut lease = self.get_conn().await?;
        let conn = lease.conn_mut()?;
        let sql = "SELECT s.SCHEMA_NAME, CAST(COALESCE(SUM(t.DATA_LENGTH + t.INDEX_LENGTH), 0) AS SIGNED) \
                   FROM information_schema.SCHEMATA s \
                   LEFT JOIN information_schema.TABLES t ON t.TABLE_SCHEMA = s.SCHEMA_NAME \
                   GROUP BY s.SCHEMA_NAME \
                   ORDER BY s.SCHEMA_NAME ASC";
        let start = Instant::now();
        match conn.query::<Row, _>(sql).await {
            Ok(rows) => {
                let dbs = rows
                    .into_iter()
                    .map(|row| DatabaseInfo {
                        name: value_to_string(row.as_ref(0).unwrap_or(&Value::NULL)),
                        owner: String::new(),
                        size_bytes: value_to_opt_i64(row.as_ref(1).unwrap_or(&Value::NULL)),
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
            Err(err) if is_mysql_permission_error(&err) => {
                let fallback_sql = "SHOW DATABASES";
                let rows: Vec<Row> = conn.query(fallback_sql).await.map_err(|e| self.map_session_err(e))?;
                let mut dbs: Vec<DatabaseInfo> = rows
                    .into_iter()
                    .map(|row| DatabaseInfo {
                        name: value_to_string(row.as_ref(0).unwrap_or(&Value::NULL)),
                        owner: String::new(),
                        size_bytes: None,
                    })
                    .collect();
                dbs.sort_by(|a, b| a.name.cmp(&b.name));
                Ok((
                    dbs,
                    ExecutedSql {
                        sql: fallback_sql.to_owned(),
                        duration_ms: start.elapsed().as_millis() as u64,
                        error: None,
                    },
                ))
            }
            Err(err) => Err(self.map_session_err(err)),
        }
    }

    async fn list_schemas(&self) -> Result<(Vec<SchemaInfo>, ExecutedSql), DbError> {
        let active_db = self.server_info.current_database.trim();
        if !active_db.is_empty() {
            return Ok((
                vec![SchemaInfo {
                    name: active_db.to_owned(),
                    is_system: is_mysql_system_schema(active_db),
                }],
                ExecutedSql {
                    sql: "SELECT DATABASE()".to_owned(),
                    duration_ms: 0,
                    error: None,
                },
            ));
        }

        let mut lease = self.get_conn().await?;
        let conn = lease.conn_mut()?;
        let sql = "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA \
                   ORDER BY (SCHEMA_NAME IN ('information_schema', 'mysql', 'performance_schema', 'sys')) ASC, \
                            SCHEMA_NAME ASC";
        let start = Instant::now();
        let rows: Vec<Row> = conn.query(sql).await.map_err(|e| self.map_session_err(e))?;
        let schemas = rows
            .into_iter()
            .map(|row| {
                let name = value_to_string(row.as_ref(0).unwrap_or(&Value::NULL));
                let is_system = is_mysql_system_schema(&name);
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
        let mut lease = self.get_conn().await?;
        let conn = lease.conn_mut()?;
        let sql = "SELECT TABLE_NAME, TABLE_TYPE, TABLE_ROWS, (DATA_LENGTH + INDEX_LENGTH) \
                   FROM information_schema.TABLES \
                   WHERE TABLE_SCHEMA = ? \
                   ORDER BY TABLE_NAME ASC";
        let start = Instant::now();
        let rows: Vec<Row> = conn.exec(sql, (schema,)).await.map_err(|e| self.map_session_err(e))?;
        let objects = rows
            .into_iter()
            .filter_map(|row| {
                let name = value_to_string(row.as_ref(0).unwrap_or(&Value::NULL));
                let table_type = value_to_string(row.as_ref(1).unwrap_or(&Value::NULL));
                let kind = match table_type.to_ascii_uppercase().as_str() {
                    "BASE TABLE" => DbObjectKind::Table,
                    "VIEW" | "SYSTEM VIEW" => DbObjectKind::View,
                    _ => return None,
                };
                Some(DbObject {
                    schema: schema.to_owned(),
                    name,
                    kind,
                    estimated_rows: value_to_opt_i64(row.as_ref(2).unwrap_or(&Value::NULL)),
                    total_bytes: value_to_opt_i64(row.as_ref(3).unwrap_or(&Value::NULL)),
                })
            })
            .collect();
        Ok((
            objects,
            ExecutedSql {
                sql: format!("{sql} -- ? = '{}'", schema.replace('\'', "''")),
                duration_ms: start.elapsed().as_millis() as u64,
                error: None,
            },
        ))
    }

    async fn list_columns(&self, schema: &str, object: &str) -> Result<(Vec<ColumnSummary>, ExecutedSql), DbError> {
        let mut lease = self.get_conn().await?;
        let conn = lease.conn_mut()?;
        let sql = "SELECT COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_KEY \
                   FROM information_schema.COLUMNS \
                   WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? \
                   ORDER BY ORDINAL_POSITION ASC";
        let start = Instant::now();
        let rows: Vec<Row> = conn.exec(sql, (schema, object)).await.map_err(|e| self.map_session_err(e))?;
        let cols = rows
            .into_iter()
            .map(|row| ColumnSummary {
                name: value_to_string(row.as_ref(0).unwrap_or(&Value::NULL)),
                data_type: value_to_string(row.as_ref(1).unwrap_or(&Value::NULL)),
                nullable: value_to_string(row.as_ref(2).unwrap_or(&Value::NULL)).eq_ignore_ascii_case("YES"),
                is_primary_key: value_to_string(row.as_ref(3).unwrap_or(&Value::NULL)).eq_ignore_ascii_case("PRI"),
            })
            .collect();
        Ok((
            cols,
            ExecutedSql {
                sql: format!(
                    "{sql} -- ? = '{}', ? = '{}'",
                    schema.replace('\'', "''"),
                    object.replace('\'', "''")
                ),
                duration_ms: start.elapsed().as_millis() as u64,
                error: None,
            },
        ))
    }

    async fn fetch_table_page(&self, req: &PageRequest) -> Result<(TablePage, Vec<ExecutedSql>), DbError> {
        let limit = req.limit.clamp(1, 5000);
        let offset = req.offset.max(0);
        let meta = self.fetch_column_metas_and_order(&req.schema, &req.table).await?;
        let columns = meta.columns;
        let foreign_keys = meta.foreign_keys;
        let stable_order = meta.stable_order;

        let mut lease = self.get_conn().await?;
        let conn = lease.conn_mut()?;
        if let Some(where_clause) = req.filter_where.as_deref()
            && !where_clause.trim().is_empty()
        {
            let mode_row: Row = match tokio::time::timeout(
                PING_TIMEOUT,
                conn.query_first::<Row, _>(MYSQL_LIVE_SESSION_MODE_SQL),
            )
            .await
            {
                Ok(Ok(Some(row))) => row,
                Ok(Ok(None)) | Err(_) => {
                    lease.discard();
                    self.mysql_grid_quote_mode
                        .store(MYSQL_QUOTE_MODE_UNKNOWN, Ordering::Release);
                    return Err(DbError::QueryFailed {
                        message: "timed out checking MySQL grid sql_mode before running filtered query"
                            .to_owned(),
                        code: None,
                        detail: None,
                        hint: None,
                    });
                }
                Ok(Err(err)) => {
                    let mapped = map_mysql_session_err(err);
                    lease.discard();
                    self.mysql_grid_quote_mode
                        .store(MYSQL_QUOTE_MODE_UNKNOWN, Ordering::Release);
                    return Err(match mapped {
                        DbError::Disconnected | DbError::ConnectionFailed(_) => DbError::QueryFailed {
                            message: format!(
                                "pooled MySQL grid connection failed while checking sql_mode: {mapped}"
                            ),
                            code: None,
                            detail: None,
                            hint: None,
                        },
                        other => other,
                    });
                }
            };
            let live_grid_mode = parse_mysql_live_mode_row(&mode_row);
            self.mysql_grid_quote_mode.store(
                pack_mysql_quote_mode(Some(live_grid_mode)),
                Ordering::Release,
            );
            crate::safety::validate_table_filter_where(
                DbEngine::Mysql,
                where_clause,
                crate::sql::SqlSessionMode::mysql(Some(live_grid_mode)),
            )
            .map_err(DbError::InvalidInput)?;
        }

        if columns.is_empty() {
            lease.return_clean();
            return Ok((
                TablePage {
                    columns,
                    foreign_keys: Vec::new(),
                    rows: Vec::new(),
                    raw_rows: Vec::new(),
                    transaction_support: TableTransactionSupport::Supported,
                    offset: 0,
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

        let mut effective_offset = offset;
        let first_batch = self
            .execute_mysql_page_query(conn, &columns, req, &stable_order, limit, effective_offset)
            .await?;
        let elapsed_ms = first_batch.log.duration_ms;
        let mut logs = vec![first_batch.log];
        let mut decoded_rows = first_batch.rows;
        let mut raw_rows = first_batch.raw_rows;
        let mut has_more = first_batch.has_more;

        let filter_trimmed = req
            .filter_where
            .as_deref()
            .map(str::trim)
            .filter(|w| !w.is_empty());
        let filter_active = filter_trimmed.is_some();

        if decoded_rows.is_empty() && effective_offset > 0 {
            let mut count_sql = format!(
                "SELECT COUNT(*) FROM {}",
                DbEngine::Mysql.qualify_ident(&req.schema, &req.table)
            );
            if let Some(trimmed) = filter_trimmed {
                count_sql.push_str(&format!(" WHERE ({trimmed})"));
            }
            let count_start = Instant::now();
            let count_row: Option<Row> = conn.query_first(&count_sql).await.map_err(|e| self.map_session_err(e))?;
            logs.push(ExecutedSql {
                sql: count_sql,
                duration_ms: count_start.elapsed().as_millis() as u64,
                error: None,
            });
            let actual_total = count_row
                .as_ref()
                .and_then(|r| value_to_opt_i64(r.as_ref(0).unwrap_or(&Value::NULL)))
                .unwrap_or(0);
            if actual_total <= 0 {
                effective_offset = 0;
                has_more = false;
            } else {
                let recovered_offset = last_page_offset(actual_total, limit);
                let rec_batch = self
                    .execute_mysql_page_query(conn, &columns, req, &stable_order, limit, recovered_offset)
                    .await?;
                logs.push(rec_batch.log);
                effective_offset = recovered_offset;
                decoded_rows = rec_batch.rows;
                raw_rows = rec_batch.raw_rows;
                has_more = rec_batch.has_more;

                if decoded_rows.is_empty() && effective_offset > 0 {
                    let zero_batch = self
                        .execute_mysql_page_query(conn, &columns, req, &stable_order, limit, 0)
                        .await?;
                    logs.push(zero_batch.log);
                    effective_offset = 0;
                    decoded_rows = zero_batch.rows;
                    raw_rows = zero_batch.raw_rows;
                    has_more = zero_batch.has_more;
                }
            }
        }

        let (transaction_support, table_rows_est, meta_log) =
            self.fetch_mysql_table_meta(conn, &req.schema, &req.table).await?;
        if let Some(log) = meta_log
            && has_more
            && !filter_active
        {
            logs.push(log);
        }

        let (total_rows_estimate, total_is_exact) = if !has_more {
            debug_assert!(!decoded_rows.is_empty() || effective_offset == 0);
            (Some(effective_offset + decoded_rows.len() as i64), true)
        } else if !filter_active {
            (table_rows_est, false)
        } else {
            (None, false)
        };

        Ok((
            TablePage {
                columns,
                foreign_keys,
                rows: decoded_rows,
                raw_rows,
                transaction_support,
                offset: effective_offset,
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
        let mut logs = Vec::new();
        let (columns, foreign_keys, col_log) = self.fetch_column_metas(schema, table).await?;
        logs.push(col_log);

        let mut lease = self.get_conn().await?;
        let conn = lease.conn_mut()?;

        let idx_sql = "SELECT INDEX_NAME, NON_UNIQUE, COLUMN_NAME, INDEX_TYPE, SEQ_IN_INDEX \
                       FROM information_schema.STATISTICS \
                       WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? \
                       ORDER BY (INDEX_NAME = 'PRIMARY') DESC, NON_UNIQUE ASC, INDEX_NAME ASC, SEQ_IN_INDEX ASC";
        let idx_start = Instant::now();
        let idx_rows: Vec<Row> = conn.exec(idx_sql, (schema, table)).await.map_err(|e| self.map_session_err(e))?;
        logs.push(ExecutedSql {
            sql: format!(
                "{idx_sql} -- ? = '{}', ? = '{}'",
                schema.replace('\'', "''"),
                table.replace('\'', "''")
            ),
            duration_ms: idx_start.elapsed().as_millis() as u64,
            error: None,
        });

        let fq_table = DbEngine::Mysql.qualify_ident(schema, table);
        let mut grouped_indexes: BTreeMap<(u8, u8, String), (bool, bool, String, Vec<String>)> = BTreeMap::new();
        for row in idx_rows {
            let name = value_to_string(row.as_ref(0).unwrap_or(&Value::NULL));
            let non_unique = value_to_opt_i64(row.as_ref(1).unwrap_or(&Value::NULL)).unwrap_or(1);
            let col_name = value_to_string(row.as_ref(2).unwrap_or(&Value::NULL));
            let method = value_to_string(row.as_ref(3).unwrap_or(&Value::NULL)).to_ascii_lowercase();
            let is_primary = name == "PRIMARY";
            let is_unique = non_unique == 0;
            let key = (u8::from(!is_primary), u8::from(!is_unique), name.clone());
            let entry = grouped_indexes
                .entry(key)
                .or_insert_with(|| (is_unique, is_primary, method, Vec::new()));
            entry.3.push(col_name);
        }

        let indexes: Vec<IndexInfo> = grouped_indexes
            .into_iter()
            .map(|((_, _, name), (is_unique, is_primary, method, cols))| {
                let quoted_cols = cols
                    .iter()
                    .map(|c| DbEngine::Mysql.quote_ident(c))
                    .collect::<Vec<_>>()
                    .join(", ");
                let definition = if is_primary {
                    format!("PRIMARY KEY ({quoted_cols})")
                } else if is_unique {
                    format!(
                        "CREATE UNIQUE INDEX {} ON {fq_table} ({quoted_cols})",
                        DbEngine::Mysql.quote_ident(&name)
                    )
                } else {
                    format!(
                        "CREATE INDEX {} ON {fq_table} ({quoted_cols})",
                        DbEngine::Mysql.quote_ident(&name)
                    )
                };
                IndexInfo {
                    name,
                    columns: cols,
                    is_unique,
                    is_primary,
                    method,
                    size_bytes: None,
                    definition,
                }
            })
            .collect();

        let stats_sql = "SELECT t.TABLE_TYPE, t.DATA_LENGTH, t.INDEX_LENGTH, t.TABLE_ROWS, \
                         t.TABLE_COMMENT, t.ENGINE, e.TRANSACTIONS \
                         FROM information_schema.TABLES t \
                         LEFT JOIN information_schema.ENGINES e ON UPPER(e.ENGINE) = UPPER(t.ENGINE) \
                         WHERE t.TABLE_SCHEMA = ? AND t.TABLE_NAME = ?";
        let stats_start = Instant::now();
        let (stats_row, used_stats_sql) = match conn.exec_first::<Row, _, _>(stats_sql, (schema, table)).await {
            Ok(row) => (row, stats_sql),
            Err(err) if is_mysql_permission_error(&err) => {
                let fallback_sql = "SELECT TABLE_TYPE, DATA_LENGTH, INDEX_LENGTH, TABLE_ROWS, \
                                    TABLE_COMMENT, ENGINE, NULL \
                                    FROM information_schema.TABLES \
                                    WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?";
                let row = conn
                    .exec_first(fallback_sql, (schema, table))
                    .await
                    .map_err(|e| self.map_session_err(e))?;
                (row, fallback_sql)
            }
            Err(err) => return Err(self.map_session_err(err)),
        };
        logs.push(ExecutedSql {
            sql: format!(
                "{used_stats_sql} -- ? = '{}', ? = '{}'",
                schema.replace('\'', "''"),
                table.replace('\'', "''")
            ),
            duration_ms: stats_start.elapsed().as_millis() as u64,
            error: None,
        });

        let (is_view, data_bytes, index_bytes, estimated_rows, comment, transaction_support) =
            if let Some(row) = stats_row {
                let table_type_raw = value_to_string(row.as_ref(0).unwrap_or(&Value::NULL));
                let table_type = table_type_raw.to_ascii_uppercase();
                let is_view = table_type.contains("VIEW");
                let data_bytes = value_to_opt_i64(row.as_ref(1).unwrap_or(&Value::NULL));
                let index_bytes = value_to_opt_i64(row.as_ref(2).unwrap_or(&Value::NULL));
                let estimated_rows = value_to_opt_i64(row.as_ref(3).unwrap_or(&Value::NULL));
                let comment = value_to_opt_string(row.as_ref(4).unwrap_or(&Value::NULL))
                    .filter(|s| !s.trim().is_empty())
                    .filter(|s| !(is_view && s.eq_ignore_ascii_case("VIEW")));
                let engine_name = value_to_opt_string(row.as_ref(5).unwrap_or(&Value::NULL));
                let tx_flag = value_to_opt_string(row.as_ref(6).unwrap_or(&Value::NULL));
                let tx_support = classify_mysql_table_transaction_support(
                    Some(table_type_raw.as_str()),
                    engine_name.as_deref(),
                    tx_flag.as_deref(),
                );
                (is_view, data_bytes, index_bytes, estimated_rows, comment, tx_support)
            } else {
                (false, None, None, None, None, TableTransactionSupport::Unknown)
            };

        let total_bytes = match (data_bytes, index_bytes) {
            (Some(d), Some(i)) => Some(d.saturating_add(i)),
            (Some(d), None) => Some(d),
            (None, Some(i)) => Some(i),
            (None, None) => None,
        };

        let show_sql = if is_view {
            format!("SHOW CREATE VIEW {fq_table}")
        } else {
            format!("SHOW CREATE TABLE {fq_table}")
        };
        let show_start = Instant::now();
        let ddl = match conn.query_first::<Row, _>(&show_sql).await {
            Ok(Some(row)) => {
                logs.push(ExecutedSql {
                    sql: show_sql,
                    duration_ms: show_start.elapsed().as_millis() as u64,
                    error: None,
                });
                let raw_ddl = value_to_opt_string(row.as_ref(1).unwrap_or(&Value::NULL))
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty());
                match raw_ddl {
                    Some(mut d) => {
                        if !d.ends_with(';') {
                            d.push(';');
                        }
                        d
                    }
                    None => synthesize_mysql_table_ddl(schema, table, &columns, &foreign_keys, &indexes),
                }
            }
            Ok(None) => {
                logs.push(ExecutedSql {
                    sql: show_sql,
                    duration_ms: show_start.elapsed().as_millis() as u64,
                    error: None,
                });
                synthesize_mysql_table_ddl(schema, table, &columns, &foreign_keys, &indexes)
            }
            Err(err) => {
                logs.push(ExecutedSql {
                    sql: show_sql,
                    duration_ms: show_start.elapsed().as_millis() as u64,
                    error: Some(err.to_string()),
                });
                synthesize_mysql_table_ddl(schema, table, &columns, &foreign_keys, &indexes)
            }
        };

        Ok((
            TableStructure {
                columns,
                foreign_keys,
                indexes,
                stats: TableStats {
                    total_bytes,
                    data_bytes,
                    index_bytes,
                    estimated_rows,
                    comment,
                    ddl,
                    transaction_support,
                },
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
            Some(expected_sql_mode.mysql),
        )
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

        let _tx_guard = self.tx_lock.lock().await;
        let mut lease = self.get_checked_conn(true, true, None).await?;
        let conn = lease.conn_mut()?;
        let total_start = Instant::now();
        let mut logs = Vec::with_capacity(statements.len() + 2);
        let mut total_rows_affected = 0u64;
        let mut executed = 0usize;

        let begin_start = Instant::now();
        if let Err(err) = conn.query_drop("START TRANSACTION").await {
            let mapped = self.map_session_err(err);
            logs.push(ExecutedSql {
                sql: "START TRANSACTION;".to_owned(),
                duration_ms: begin_start.elapsed().as_millis() as u64,
                error: Some(mapped.to_string()),
            });
            return Err(mapped);
        }
        logs.push(ExecutedSql {
            sql: "START TRANSACTION;".to_owned(),
            duration_ms: begin_start.elapsed().as_millis() as u64,
            error: None,
        });

        for raw_stmt in statements {
            let stmt = crate::sql::trim_sql_whitespace_for_engine(DbEngine::Mysql, raw_stmt);
            if stmt.is_empty() {
                continue;
            }
            let stmt_start = Instant::now();
            match conn.query_drop(stmt).await {
                Ok(()) => {
                    total_rows_affected = total_rows_affected.saturating_add(conn.affected_rows());
                    executed += 1;
                    logs.push(ExecutedSql {
                        sql: stmt.to_owned(),
                        duration_ms: stmt_start.elapsed().as_millis() as u64,
                        error: None,
                    });
                }
                Err(err) => {
                    let mapped = self.map_session_err(err);
                    drop(conn.query_drop("ROLLBACK").await);
                    return Err(mapped);
                }
            }
        }

        let commit_start = Instant::now();
        match conn.query_drop("COMMIT").await {
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
                let mapped = self.map_session_err(err);
                drop(conn.query_drop("ROLLBACK").await);
                Err(mapped)
            }
        }
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

        let target_schema = if batch.schema.trim().is_empty() {
            self.server_info.current_database.trim().to_owned()
        } else {
            batch.schema.clone()
        };
        if target_schema.is_empty() {
            return Err(DbError::NonTransactionalTable {
                support: TableTransactionSupport::Unknown,
                message: "Cannot verify storage engine transaction support without an active schema".to_owned(),
            });
        }

        let _tx_guard = self.tx_lock.lock().await;
        let mut lease = self.get_checked_conn(true, true, None).await?;
        let conn = lease.conn_mut()?;
        let total_start = Instant::now();
        let mut logs = Vec::with_capacity(batch.mutations.len() + 4);

        let (tx_support, _, meta_log) = self
            .fetch_mysql_table_meta(conn, &target_schema, &batch.table)
            .await?;
        if let Some(log) = meta_log {
            logs.push(log);
        }
        ensure_mysql_table_transactional(tx_support, &target_schema, &batch.table)?;

        let mut plans = Vec::with_capacity(batch.mutations.len());
        for mutation in &batch.mutations {
            plans.push(build_mysql_staged_mutation_plan(&target_schema, &batch.table, mutation)?);
        }

        let begin_start = Instant::now();
        if let Err(err) = conn.query_drop("START TRANSACTION").await {
            let mapped = self.map_session_err(err);
            logs.push(ExecutedSql {
                sql: "START TRANSACTION;".to_owned(),
                duration_ms: begin_start.elapsed().as_millis() as u64,
                error: Some(mapped.to_string()),
            });
            return Err(mapped);
        }
        logs.push(ExecutedSql {
            sql: "START TRANSACTION;".to_owned(),
            duration_ms: begin_start.elapsed().as_millis() as u64,
            error: None,
        });

        // Acquire transactional MDL with plain SELECT (works even without UPDATE/LOCK TABLES grants),
        // then re-verify the storage engine before any write so concurrent ALTER TABLE cannot slip through.
        let fq_target = DbEngine::Mysql.qualify_ident(&target_schema, &batch.table);
        if let Err(err) = conn
            .query_first::<Row, _>(format!("SELECT 1 FROM {fq_target} WHERE 1 = 0"))
            .await
        {
            let mapped = self.map_session_err(err);
            drop(conn.query_drop("ROLLBACK").await);
            return Err(mapped);
        }
        let (post_mdl_support, _, post_mdl_log) = match self
            .fetch_mysql_table_meta(conn, &target_schema, &batch.table)
            .await
        {
            Ok(meta) => meta,
            Err(err) => {
                drop(conn.query_drop("ROLLBACK").await);
                return Err(err);
            }
        };
        if let Some(log) = post_mdl_log {
            logs.push(log);
        }
        if let Err(err) = ensure_mysql_table_transactional(post_mdl_support, &target_schema, &batch.table) {
            drop(conn.query_drop("ROLLBACK").await);
            return Err(err);
        }

        let mut total_rows_affected = 0u64;
        let mut executed = 0usize;
        for plan in plans {
            let (op_kind, row_label, lock_opt, exec_sql, exec_params, require_exact_affected) = match plan {
                MysqlMutationPlan::Update {
                    row_label,
                    lock_sql,
                    lock_params,
                    exec_sql,
                    exec_params,
                } => ("update", row_label, Some((lock_sql, lock_params)), exec_sql, exec_params, false),
                MysqlMutationPlan::Insert {
                    row_label,
                    exec_sql,
                    exec_params,
                } => ("insert", row_label, None, exec_sql, exec_params, true),
                MysqlMutationPlan::Delete {
                    row_label,
                    lock_sql,
                    lock_params,
                    exec_sql,
                    exec_params,
                } => ("delete", row_label, Some((lock_sql, lock_params)), exec_sql, exec_params, true),
            };

            if let Some((lock_sql, lock_params)) = lock_opt {
                let count_row: Option<Row> = match conn
                    .exec_first(lock_sql, Params::Positional(lock_params))
                    .await
                {
                    Ok(row) => row,
                    Err(err) => {
                        let mapped = self.map_session_err(err);
                        drop(conn.query_drop("ROLLBACK").await);
                        return Err(mapped);
                    }
                };
                let matched = count_row
                    .as_ref()
                    .and_then(|r| r.as_ref(0))
                    .and_then(value_to_opt_i64)
                    .unwrap_or(0);
                if matched != 1 {
                    drop(conn.query_drop("ROLLBACK").await);
                    return Err(DbError::StaleRowConflict(stale_conflict_msg(
                        op_kind, &row_label, matched.max(0) as u64,
                    )));
                }
            }

            let stmt_start = Instant::now();
            match conn.exec_drop(&exec_sql, Params::Positional(exec_params)).await {
                Ok(()) => {
                    let affected = conn.affected_rows();
                    if (require_exact_affected && affected != 1) || affected > 1 {
                        drop(conn.query_drop("ROLLBACK").await);
                        return Err(DbError::StaleRowConflict(stale_conflict_msg(
                            op_kind, &row_label, affected,
                        )));
                    }
                    total_rows_affected = total_rows_affected.saturating_add(affected);
                    executed += 1;
                    logs.push(ExecutedSql {
                        sql: exec_sql,
                        duration_ms: stmt_start.elapsed().as_millis() as u64,
                        error: None,
                    });
                }
                Err(err) => {
                    let mapped = self.map_session_err(err);
                    drop(conn.query_drop("ROLLBACK").await);
                    return Err(mapped);
                }
            }
        }

        let commit_start = Instant::now();
        match conn.query_drop("COMMIT").await {
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
                let mapped = self.map_session_err(err);
                drop(conn.query_drop("ROLLBACK").await);
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
        self.explain_sql_inner(statement, analyze, Some(expected_sql_mode.mysql))
            .await
    }

    async fn fetch_fk_row(
        &self,
        fk: &ForeignKeyRef,
        components: &[FkLookupComponent],
    ) -> Result<(FkPeekRow, ExecutedSql), DbError> {
        let Some(where_clause) = build_fk_where_clause_for_engine(DbEngine::Mysql, fk, components) else {
            return Err(DbError::InvalidInput(
                "cannot peek unresolved or NULL foreign-key value".to_owned(),
            ));
        };

        let target_schema = if fk.target_schema.trim().is_empty() {
            self.server_info.current_database.as_str()
        } else {
            fk.target_schema.as_str()
        };

        let meta = self
            .fetch_column_metas_and_order(target_schema, &fk.target_table)
            .await?;
        let target_columns = meta.columns;
        let stable_order = meta.stable_order;
        if target_columns.is_empty() {
            return Err(DbError::InvalidInput(format!(
                "target table {target_schema}.{} has no columns",
                fk.target_table
            )));
        }
        for target_col in &fk.target_columns {
            if !target_columns.iter().any(|c| c.name == *target_col) {
                return Err(DbError::InvalidInput(format!(
                    "target table {target_schema}.{} is missing referenced column {target_col}",
                    fk.target_table
                )));
            }
        }

        let req = PageRequest {
            schema: target_schema.to_owned(),
            table: fk.target_table.clone(),
            filter_where: Some(where_clause),
            sort: None,
            limit: 1,
            offset: 0,
        };
        let sql = build_mysql_page_sql(
            target_schema,
            &fk.target_table,
            &target_columns,
            &req,
            &stable_order,
            1,
            0,
        );

        let mut lease = self.get_conn().await?;
        let conn = lease.conn_mut()?;
        let start = Instant::now();
        let first_row: Option<Row> = conn.query_first(&sql).await.map_err(|e| self.map_session_err(e))?;
        let elapsed_ms = start.elapsed().as_millis() as u64;

        let row = first_row.map(|r| {
            let wire_cols = r.columns_ref();
            target_columns
                .iter()
                .enumerate()
                .map(|(idx, col)| {
                    let val = r.as_ref(idx).unwrap_or(&Value::NULL);
                    decode_mysql_cell(val, wire_cols.get(idx), Some(col))
                })
                .collect()
        });

        Ok((
            FkPeekRow {
                fk: fk.clone(),
                lookup_display: format_fk_lookup_display(components),
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

    async fn cancel_running_query(&self) -> Result<(), DbError> {
        let target = self.active_conn_ids.lock().ok().and_then(|guard| {
            let (&key, entry) = guard.get_key_value(&0).or_else(|| {
                if guard.len() == 1 {
                    guard.iter().next()
                } else {
                    None
                }
            })?;
            entry.cancel_requested.store(true, Ordering::Release);
            Some((key, entry.conn_id))
        });
        let Some((cancel_key, conn_id)) = target else {
            return Ok(());
        };
        self.kill_query_conn_id(conn_id, Some(cancel_key)).await
    }

    async fn cancel_query_by_key(&self, cancel_key: u64) -> Result<(), DbError> {
        let conn_id = self.active_conn_ids.lock().ok().and_then(|guard| {
            let entry = guard.get(&cancel_key)?;
            entry.cancel_requested.store(true, Ordering::Release);
            Some(entry.conn_id)
        });
        let Some(conn_id) = conn_id else {
            return Ok(());
        };
        self.kill_query_conn_id(conn_id, Some(cancel_key)).await
    }

    async fn ping(&self) -> Result<(), DbError> {
        let lease = self.get_checked_conn(false, false, None).await?;
        lease.return_clean();
        Ok(())
    }
}

fn evaluate_mid_batch_mysql_mode(
    actual_mode: crate::sql::MysqlQuoteMode,
    expected_mode: Option<crate::sql::MysqlQuoteMode>,
    next_stmt: &str,
) -> Result<(), String> {
    if let Some(exp) = expected_mode
        && evaluate_live_mysql_mode(actual_mode, Some(exp)).is_err()
    {
        return Err(
            "MySQL session quote mode or server version changed mid-batch; remaining statements skipped for safety"
                .to_owned(),
        );
    }
    if crate::sql::mysql_slice_has_multiple_statements(next_stmt, Some(actual_mode)) {
        return Err(
            "MySQL statement expands to multiple semicolon-separated statements under the live session mode; remaining statements skipped for safety"
                .to_owned(),
        );
    }
    Ok(())
}

impl MysqlSession {
    async fn explain_sql_inner(
        &self,
        statement: &str,
        analyze: bool,
        expected_mysql_mode: Option<Option<crate::sql::MysqlQuoteMode>>,
    ) -> Result<(ExplainPlan, ExecutedSql), DbError> {
        let trimmed = crate::sql::trim_sql_whitespace_for_engine(DbEngine::Mysql, statement);
        if trimmed.is_empty() {
            return Err(DbError::InvalidInput(
                "cannot explain an empty SQL statement".to_owned(),
            ));
        }

        let explain_query = build_explain_sql_for_engine(DbEngine::Mysql, trimmed, analyze);
        let mut editor_guard = self.editor_conn.lock().await;
        let mut conn = self.acquire_editor_conn(&mut editor_guard, None).await?;
        let mut live_mode_for_safety = self.mysql_quote_mode();
        if let Some(expected) = expected_mysql_mode {
            let precheck_res: Result<crate::sql::MysqlQuoteMode, DbError> = async {
                let live_mode = self.verify_editor_mysql_mode(&mut conn, expected, None).await?;
                if crate::sql::mysql_slice_has_multiple_statements(trimmed, Some(live_mode)) {
                    return Err(DbError::InvalidInput(
                        "cannot EXPLAIN a MySQL slice containing multiple statements; select or split a single statement"
                            .to_owned(),
                    ));
                }
                Ok(live_mode)
            }
            .await;
            match precheck_res {
                Ok(mode) => live_mode_for_safety = Some(mode),
                Err(err) => {
                    if self.is_disconnected()
                        || matches!(err, DbError::DisconnectedBeforeDispatch(_) | DbError::Disconnected)
                    {
                        spawn_disconnect_mysql_conn(conn);
                        self.disconnected.store(true, Ordering::Release);
                        self.mysql_quote_mode
                            .store(MYSQL_QUOTE_MODE_UNKNOWN, Ordering::Release);
                    } else {
                        *editor_guard = Some(conn);
                    }
                    return Err(err);
                }
            }
        } else if self.read_only
            && crate::sql::mysql_slice_has_multiple_statements(trimmed, live_mode_for_safety)
        {
            *editor_guard = Some(conn);
            return Err(DbError::InvalidInput(
                "cannot EXPLAIN a MySQL slice containing multiple statements; select or split a single statement"
                    .to_owned(),
            ));
        }
        if self.read_only && analyze {
            let decision = crate::safety::evaluate_sql_safety_for_engine_with_session_mode(
                DbEngine::Mysql,
                SafetyPolicy::ReadOnly,
                EnvTag::Local,
                std::slice::from_ref(&explain_query),
                crate::sql::SqlSessionMode::mysql(live_mode_for_safety),
            );
            if let crate::safety::SafetyDecision::Block { reason, .. } = decision {
                *editor_guard = Some(conn);
                return Err(DbError::InvalidInput(reason));
            }
        }
        if self.read_only
            && let Err(err) = conn.query_drop("START TRANSACTION READ ONLY").await
        {
            let mapped = self.map_session_err(err);
            if self.is_disconnected() || matches!(mapped, DbError::Disconnected) {
                spawn_disconnect_mysql_conn(conn);
                self.disconnected.store(true, Ordering::Release);
                self.mysql_quote_mode
                    .store(MYSQL_QUOTE_MODE_UNKNOWN, Ordering::Release);
            } else {
                *editor_guard = Some(conn);
            }
            return Err(mapped);
        }
        let start = Instant::now();
        let query_res = conn.query::<Row, _>(&explain_query).await;
        if self.read_only
            && let Err(rb_err) = conn.query_drop("ROLLBACK").await
        {
            let mapped = self.map_session_err(rb_err);
            spawn_disconnect_mysql_conn(conn);
            self.disconnected.store(true, Ordering::Release);
            self.mysql_quote_mode
                .store(MYSQL_QUOTE_MODE_UNKNOWN, Ordering::Release);
            return Err(if matches!(mapped, DbError::Disconnected) {
                mapped
            } else {
                DbError::Disconnected
            });
        }
        let rows = match query_res {
            Ok(rows) => {
                *editor_guard = Some(conn);
                rows
            }
            Err(err) => {
                let mapped = self.map_session_err(err);
                if self.is_disconnected() || matches!(mapped, DbError::Disconnected) {
                    spawn_disconnect_mysql_conn(conn);
                    self.disconnected.store(true, Ordering::Release);
                    self.mysql_quote_mode
                        .store(MYSQL_QUOTE_MODE_UNKNOWN, Ordering::Release);
                } else {
                    *editor_guard = Some(conn);
                }
                return Err(mapped);
            }
        };
        let duration_ms = start.elapsed().as_millis() as u64;

        let mut chunks = Vec::with_capacity(rows.len());
        for row in rows {
            if let Some(val) = row.as_ref(0)
                && !matches!(val, Value::NULL)
            {
                chunks.push(value_to_string(val));
            }
        }
        let raw_output = chunks.join("\n");

        let plan = if analyze {
            ExplainPlan::parse_mysql_analyze_tree(trimmed.to_owned(), &raw_output)
        } else {
            ExplainPlan::parse_mysql_json(trimmed.to_owned(), &raw_output)
        }
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
        expected_mysql_mode: Option<Option<crate::sql::MysqlQuoteMode>>,
    ) -> Result<(QueryBatchResult, Vec<ExecutedSql>), DbError> {
        let is_cancelled = || cancel_flag.is_some_and(|f| f.load(Ordering::Acquire));
        let mut editor_guard = self.editor_conn.lock().await;
        let mut conn = self.acquire_editor_conn(&mut editor_guard, cancel_flag).await?;
        let verified_live_mode = if let Some(expected) = expected_mysql_mode {
            let verify_res: Result<crate::sql::MysqlQuoteMode, DbError> = async {
                let live_mode = self
                    .verify_editor_mysql_mode(&mut conn, expected, cancel_flag)
                    .await?;
                if let Err(msg) =
                    crate::sql::validate_mysql_single_statement_batch(statements, Some(live_mode))
                {
                    return Err(DbError::InvalidInput(msg));
                }
                Ok(live_mode)
            }
            .await;
            match verify_res {
                Ok(live_mode) => Some(live_mode),
                Err(err) => {
                    if self.is_disconnected()
                        || matches!(err, DbError::DisconnectedBeforeDispatch(_) | DbError::Disconnected)
                    {
                        spawn_disconnect_mysql_conn(conn);
                        self.disconnected.store(true, Ordering::Release);
                        self.mysql_quote_mode
                            .store(MYSQL_QUOTE_MODE_UNKNOWN, Ordering::Release);
                    } else {
                        *editor_guard = Some(conn);
                    }
                    return Err(err);
                }
            }
        } else {
            None
        };
        if self.read_only {
            let live_mode = verified_live_mode.or_else(|| self.mysql_quote_mode());
            if expected_mysql_mode.is_none()
                && let Err(msg) = crate::sql::validate_mysql_single_statement_batch(statements, live_mode)
            {
                *editor_guard = Some(conn);
                return Err(DbError::InvalidInput(msg));
            }
            let decision = crate::safety::evaluate_sql_safety_for_engine_with_session_mode(
                DbEngine::Mysql,
                SafetyPolicy::ReadOnly,
                EnvTag::Local,
                statements,
                crate::sql::SqlSessionMode::mysql(live_mode),
            );
            if let crate::safety::SafetyDecision::Block { reason, .. } = decision {
                *editor_guard = Some(conn);
                return Err(DbError::InvalidInput(reason));
            }
        }
        let conn_id = u64::from(conn.id());
        let cancel_requested = Arc::new(AtomicBool::new(false));
        if let Ok(mut guard) = self.active_conn_ids.lock() {
            guard.insert(
                cancel_key,
                ActiveMysqlQuery {
                    conn_id,
                    cancel_requested: Arc::clone(&cancel_requested),
                },
            );
        }
        let active_guard = ActiveConnMapGuard {
            map: &self.active_conn_ids,
            cancel_key,
        };

        let mut checked_mode: Option<Option<crate::sql::MysqlQuoteMode>> =
            verified_live_mode.map(Some);
        let mut need_mid_batch_mode_recheck = false;
        let mut batch_may_change_editor_sql_mode = statements
            .iter()
            .any(|s| crate::sql::mysql_stmt_may_touch_sql_mode_text(s));
        let batch_may_change_global_sql_mode = statements
            .iter()
            .any(|s| mysql_stmt_may_touch_global_sql_mode_text(s));
        let mut refreshed_mode = false;
        let is_cancel_pending = || {
            should_abort_mysql_stmt_dispatch(
                is_cancelled(),
                cancel_requested.load(Ordering::Acquire),
            )
        };
        let mut discard_lease = false;
        let mut server_confirmed_cancel = false;
        let mut protocol_dirty = false;
        let mut outcome: Result<(QueryBatchResult, Vec<ExecutedSql>), DbError> = async {
            if is_cancel_pending() {
                return Err(DbError::Cancelled);
            }

            let batch_start = Instant::now();
            let mut results = Vec::new();
            let mut logs = Vec::new();
            let mut cancelled = false;

            for raw_stmt in statements {
                let stmt = crate::sql::trim_sql_whitespace_for_engine(DbEngine::Mysql, raw_stmt);
                if stmt.is_empty() {
                    continue;
                }
                if is_cancel_pending() {
                    if results.is_empty() {
                        return Err(DbError::Cancelled);
                    }
                    cancelled = true;
                    break;
                }
                if need_mid_batch_mode_recheck
                    && let Some(expected_for_stmt) = checked_mode
                {
                    let probe_res = tokio::time::timeout(
                        PING_TIMEOUT,
                        conn.query_first::<Row, _>(MYSQL_LIVE_SESSION_MODE_SQL),
                    )
                    .await;
                    enum ProbeOutcome {
                        Ready(crate::sql::MysqlQuoteMode),
                        TimedOutOrEmpty,
                        MappedErr(DbError),
                    }
                    let (outcome_kind, failure_kind) = match probe_res {
                        Ok(Ok(Some(mode_row))) => (
                            ProbeOutcome::Ready(parse_mysql_live_mode_row(&mode_row)),
                            None,
                        ),
                        Ok(Ok(None)) | Err(_) => (
                            ProbeOutcome::TimedOutOrEmpty,
                            Some(MidBatchProbeFailure::TimedOutOrEmpty),
                        ),
                        Ok(Err(err)) => {
                            if !matches!(err, MysqlError::Server(_)) {
                                protocol_dirty = true;
                            }
                            let mapped = self.map_session_err(err);
                            let failure = match mapped {
                                DbError::Disconnected => MidBatchProbeFailure::Disconnected,
                                DbError::Cancelled => {
                                    server_confirmed_cancel = true;
                                    MidBatchProbeFailure::Cancelled
                                }
                                _ => MidBatchProbeFailure::QueryError,
                            };
                            (ProbeOutcome::MappedErr(mapped), Some(failure))
                        }
                    };
                    // Record disconnect/discard flags BEFORE aborting on cancellation so
                    // timed-out + cancelled probes still discard the lease and skip reconnect probes (R19-5).
                    let decision =
                        decide_mid_batch_probe_after_wait(failure_kind, is_cancel_pending());
                    if decision.mark_disconnected {
                        self.disconnected.store(true, Ordering::Release);
                    }
                    if decision.discard_lease {
                        discard_lease = true;
                    }
                    if decision.abort_cancelled {
                        if results.is_empty() {
                            return Err(DbError::Cancelled);
                        }
                        cancelled = true;
                        break;
                    }
                    match outcome_kind {
                        ProbeOutcome::Ready(actual_mode) => {
                            if let Err(err_msg) =
                                evaluate_mid_batch_mysql_mode(actual_mode, expected_for_stmt, stmt)
                            {
                                let err_sql = stmt.to_owned();
                                logs.push(ExecutedSql {
                                    sql: err_sql.clone(),
                                    duration_ms: 0,
                                    error: Some(err_msg.clone()),
                                });
                                results.push(QueryStatementResult {
                                    sql: err_sql,
                                    columns: Vec::new(),
                                    rows: Vec::new(),
                                    rows_affected: None,
                                    truncated: false,
                                    total_rows_seen: 0,
                                    elapsed_ms: 0,
                                    error: Some(err_msg),
                                });
                                batch_may_change_editor_sql_mode = true;
                                break;
                            }
                            need_mid_batch_mode_recheck = false;
                        }
                        ProbeOutcome::TimedOutOrEmpty => {
                            if results.is_empty() {
                                return Err(DbError::Disconnected);
                            }
                            let err_msg = DbError::Disconnected.to_string();
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
                        ProbeOutcome::MappedErr(mapped) => {
                            if matches!(mapped, DbError::Disconnected) && results.is_empty() {
                                return Err(DbError::Disconnected);
                            }
                            let err_msg = mapped.to_string();
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
                }
                if is_cancel_pending() {
                    if results.is_empty() {
                        return Err(DbError::Cancelled);
                    }
                    cancelled = true;
                    break;
                }
                let start = Instant::now();
                let mut ro_tx_started = false;
                if self.read_only {
                    match conn.query_drop("START TRANSACTION READ ONLY").await {
                        Ok(()) => ro_tx_started = true,
                        Err(mysql_err) => {
                            let elapsed_ms = start.elapsed().as_millis() as u64;
                            if !matches!(mysql_err, MysqlError::Server(_)) {
                                protocol_dirty = true;
                            }
                            let mapped = self.map_session_err(mysql_err);
                            if matches!(mapped, DbError::Cancelled) {
                                cancelled = true;
                                server_confirmed_cancel = true;
                            }
                            if matches!(mapped, DbError::Disconnected) && results.is_empty() {
                                return Err(DbError::Disconnected);
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

                match conn.query_iter(stmt).await {
                    Ok(mut result) => {
                        let wire_cols = result.columns().unwrap_or_else(|| Arc::from([]));
                        let columns: Vec<ColumnMeta> = wire_cols.iter().map(column_meta_from_wire_column).collect();
                        let mut rows: Vec<Vec<CellValue>> = Vec::new();
                        let mut truncated = false;
                        let mut total_rows_seen = 0usize;
                        let mut stream_err: Option<MysqlError> = None;

                        loop {
                            match result.next().await {
                                Ok(Some(row)) => {
                                    total_rows_seen += 1;
                                    if row_limit.is_none_or(|lim| rows.len() < lim) {
                                        let mut decoded = Vec::with_capacity(wire_cols.len());
                                        for (idx, wire_col) in wire_cols.iter().enumerate() {
                                            let val = row.as_ref(idx).unwrap_or(&Value::NULL);
                                            decoded.push(decode_mysql_cell(val, Some(wire_col), None));
                                        }
                                        rows.push(decoded);
                                    } else {
                                        truncated = true;
                                    }
                                }
                                Ok(None) => break,
                                Err(err) => {
                                    stream_err = Some(err);
                                    break;
                                }
                            }
                        }

                        if stream_err.is_none() {
                            if let Err(err) = result.drop_result().await {
                                stream_err = Some(err);
                            }
                        } else if matches!(stream_err, Some(MysqlError::Server(_))) {
                            if let Err(err) = result.drop_result().await
                                && !matches!(err, MysqlError::Server(_))
                            {
                                protocol_dirty = true;
                            }
                        } else {
                            protocol_dirty = true;
                            drop(result);
                        }

                        let elapsed_ms = start.elapsed().as_millis() as u64;
                        if let Some(err) = stream_err {
                            if !matches!(err, MysqlError::Server(_)) {
                                protocol_dirty = true;
                            }
                            let mapped = self.map_session_err(err);
                            if matches!(mapped, DbError::Cancelled) {
                                cancelled = true;
                                server_confirmed_cancel = true;
                            }
                            if matches!(mapped, DbError::Disconnected) && results.is_empty() {
                                if ro_tx_started {
                                    drop(conn.query_drop("ROLLBACK").await);
                                }
                                return Err(DbError::Disconnected);
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
                            if ro_tx_started
                                && let Err(rb_err) = conn.query_drop("ROLLBACK").await
                                && !matches!(rb_err, MysqlError::Server(_))
                            {
                                protocol_dirty = true;
                            }
                            break;
                        }

                        if ro_tx_started
                            && let Err(rb_err) = conn.query_drop("ROLLBACK").await
                        {
                            if !matches!(rb_err, MysqlError::Server(_)) {
                                protocol_dirty = true;
                            }
                            drop(self.map_session_err(rb_err));
                        }

                        let rows_affected = if columns.is_empty() {
                            Some(conn.affected_rows())
                        } else {
                            None
                        };

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
                        if let Some(cur_expected) = checked_mode {
                            if let Some(cur) = cur_expected {
                                match crate::sql::parse_inline_mysql_set_sql_mode(stmt, cur, true) {
                                    Some(Some(next)) => {
                                        checked_mode = Some(Some(next));
                                        need_mid_batch_mode_recheck = true;
                                    }
                                    Some(None) => {
                                        checked_mode = Some(None);
                                        need_mid_batch_mode_recheck = true;
                                    }
                                    None if crate::sql::mysql_stmt_may_touch_sql_mode_text(stmt) => {
                                        need_mid_batch_mode_recheck = true;
                                    }
                                    None => {}
                                }
                            } else {
                                need_mid_batch_mode_recheck = true;
                            }
                        }
                    }
                    Err(mysql_err) => {
                        let elapsed_ms = start.elapsed().as_millis() as u64;
                        if !matches!(mysql_err, MysqlError::Server(_)) {
                            protocol_dirty = true;
                        }
                        if ro_tx_started
                            && let Err(rb_err) = conn.query_drop("ROLLBACK").await
                            && !matches!(rb_err, MysqlError::Server(_))
                        {
                            protocol_dirty = true;
                        }
                        let mapped = self.map_session_err(mysql_err);
                        if matches!(mapped, DbError::Cancelled) {
                            cancelled = true;
                            server_confirmed_cancel = true;
                        }
                        if matches!(mapped, DbError::Disconnected) && results.is_empty() {
                            return Err(DbError::Disconnected);
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

            if batch_may_change_editor_sql_mode {
                if should_refresh_post_batch_global_mode(
                    true,
                    cancelled,
                    is_cancel_pending(),
                    self.is_disconnected() || discard_lease,
                ) {
                    // Persist @@SESSION.sql_mode across Run Current calls on the dedicated editor connection.
                    match tokio::time::timeout(
                        PING_TIMEOUT,
                        conn.query_first::<Row, _>(MYSQL_LIVE_SESSION_MODE_SQL),
                    )
                    .await
                    {
                        Ok(Ok(Some(mode_row))) => {
                            let refreshed = parse_mysql_live_mode_row(&mode_row);
                            self.mysql_quote_mode
                                .store(pack_mysql_quote_mode(Some(refreshed)), Ordering::Release);
                            refreshed_mode = true;
                        }
                        Ok(Ok(None)) | Err(_) => {
                            discard_lease = true;
                            self.disconnected.store(true, Ordering::Release);
                            self.mysql_quote_mode
                                .store(MYSQL_QUOTE_MODE_UNKNOWN, Ordering::Release);
                        }
                        Ok(Err(err)) => {
                            drop(self.map_session_err(err));
                            discard_lease = true;
                            self.mysql_quote_mode
                                .store(MYSQL_QUOTE_MODE_UNKNOWN, Ordering::Release);
                        }
                    }
                } else {
                    self.mysql_quote_mode
                        .store(MYSQL_QUOTE_MODE_UNKNOWN, Ordering::Release);
                }
            }
            if batch_may_change_global_sql_mode {
                // Evict previously pooled idle connections only when global/procedure mode may have changed.
                self.mysql_grid_quote_mode
                    .store(MYSQL_QUOTE_MODE_UNKNOWN, Ordering::Release);
                self.evict_idle_conns();
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

        drop(active_guard);
        let disconnected = self.is_disconnected() || discard_lease;
        let cancel_requested_flag = cancel_requested.load(Ordering::Acquire);
        let batch_cancelled = is_cancelled()
            || cancel_requested_flag
            || matches!(outcome, Ok((ref batch, _)) if batch.cancelled)
            || matches!(outcome, Err(DbError::Cancelled));
        let needs_mode_refresh = matches!(outcome, Err(DbError::Cancelled)) || !refreshed_mode;
        let mut retire_conn = should_retire_mysql_editor_conn(
            cancel_requested_flag,
            server_confirmed_cancel,
            disconnected,
            protocol_dirty,
        );
        if !retire_conn
            && (server_confirmed_cancel || batch_cancelled)
            && !matches!(tokio::time::timeout(PING_TIMEOUT, conn.ping()).await, Ok(Ok(())))
        {
            retire_conn = true;
        }
        if retire_conn {
            spawn_disconnect_mysql_conn(conn);
            self.disconnected.store(true, Ordering::Release);
            self.mysql_quote_mode
                .store(MYSQL_QUOTE_MODE_UNKNOWN, Ordering::Release);
            if batch_may_change_global_sql_mode {
                self.mysql_grid_quote_mode
                    .store(MYSQL_QUOTE_MODE_UNKNOWN, Ordering::Release);
                self.evict_idle_conns();
            }
            if !matches!(outcome, Err(DbError::Disconnected)) {
                outcome = Err(DbError::Disconnected);
            }
        } else {
            let mut mode_probe_failed = false;
            if batch_may_change_global_sql_mode {
                self.mysql_grid_quote_mode
                    .store(MYSQL_QUOTE_MODE_UNKNOWN, Ordering::Release);
                self.evict_idle_conns();
            }
            if batch_may_change_editor_sql_mode && needs_mode_refresh {
                self.mysql_quote_mode
                    .store(MYSQL_QUOTE_MODE_UNKNOWN, Ordering::Release);
                match tokio::time::timeout(
                    PING_TIMEOUT,
                    conn.query_first::<Row, _>(MYSQL_LIVE_SESSION_MODE_SQL),
                )
                .await
                {
                    Ok(Ok(Some(mode_row))) => {
                        let refreshed = parse_mysql_live_mode_row(&mode_row);
                        self.mysql_quote_mode
                            .store(pack_mysql_quote_mode(Some(refreshed)), Ordering::Release);
                    }
                    Ok(Ok(None)) | Err(_) => {
                        mode_probe_failed = true;
                    }
                    Ok(Err(err)) => {
                        drop(self.map_session_err(err));
                        mode_probe_failed = true;
                    }
                }
            }
            if mode_probe_failed {
                spawn_disconnect_mysql_conn(conn);
                self.disconnected.store(true, Ordering::Release);
                self.mysql_quote_mode
                    .store(MYSQL_QUOTE_MODE_UNKNOWN, Ordering::Release);
                if !matches!(outcome, Err(DbError::Disconnected)) {
                    outcome = Err(DbError::Disconnected);
                }
            } else {
                *editor_guard = Some(conn);
            }
        }
        outcome
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MidBatchProbeFailure {
    TimedOutOrEmpty,
    Disconnected,
    Cancelled,
    QueryError,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MidBatchProbeDecision {
    mark_disconnected: bool,
    discard_lease: bool,
    abort_cancelled: bool,
}

fn mysql_stmt_may_touch_global_sql_mode_text(stmt_sql: &str) -> bool {
    if !crate::sql::mysql_stmt_may_touch_sql_mode_text(stmt_sql) {
        return false;
    }
    let lower = stmt_sql.to_ascii_lowercase();
    lower.contains("global")
        || lower.contains("persist")
        || lower.contains("call")
        || lower.contains("execute")
}

fn decide_mid_batch_probe_after_wait(
    failure: Option<MidBatchProbeFailure>,
    cancel_pending: bool,
) -> MidBatchProbeDecision {
    let mark_disconnected = matches!(
        failure,
        Some(MidBatchProbeFailure::TimedOutOrEmpty | MidBatchProbeFailure::Disconnected)
    );
    let discard_lease = mark_disconnected;
    let abort_cancelled =
        cancel_pending || matches!(failure, Some(MidBatchProbeFailure::Cancelled));
    MidBatchProbeDecision {
        mark_disconnected,
        discard_lease,
        abort_cancelled,
    }
}

fn should_abort_mysql_stmt_dispatch(local_cancelled: bool, cancel_requested: bool) -> bool {
    local_cancelled || cancel_requested
}

fn should_refresh_post_batch_global_mode(
    batch_may_change_sql_mode: bool,
    cancelled: bool,
    cancel_pending: bool,
    disconnected: bool,
) -> bool {
    batch_may_change_sql_mode && !cancelled && !cancel_pending && !disconnected
}

fn should_retire_mysql_editor_conn(
    cancel_requested: bool,
    server_confirmed_cancel: bool,
    disconnected: bool,
    protocol_dirty: bool,
) -> bool {
    disconnected || protocol_dirty || (cancel_requested && !server_confirmed_cancel)
}

impl MysqlSession {

    async fn kill_query_conn_id(&self, conn_id: u64, cancel_key: Option<u64>) -> Result<(), DbError> {
        let mut cancel_conn = match tokio::time::timeout(
            CONNECT_TIMEOUT,
            mysql_async::Conn::new(self.opts.clone()),
        )
        .await
        {
            Ok(Ok(c)) => c,
            Ok(Err(err)) => return Err(map_mysql_err(err)),
            Err(_) => return Err(DbError::ConnectionFailed("cancel connection timed out".to_owned())),
        };

        if let Some(key) = cancel_key {
            let still_owns = self
                .active_conn_ids
                .lock()
                .ok()
                .is_some_and(|guard| guard.get(&key).is_some_and(|e| e.conn_id == conn_id));
            if !still_owns {
                drop(tokio::time::timeout(Duration::from_millis(500), cancel_conn.disconnect()).await);
                return Ok(());
            }
        }

        let kill_sql = format!("KILL QUERY {conn_id}");
        let res = match tokio::time::timeout(PING_TIMEOUT, cancel_conn.query_drop(kill_sql)).await {
            Ok(Ok(())) => Ok(()),
            // ER_NO_SUCH_THREAD: query completed before KILL QUERY arrived
            Ok(Err(MysqlError::Server(se))) if se.code == 1094 => Ok(()),
            Ok(Err(err)) => Err(map_mysql_err(err)),
            Err(_) => Err(DbError::ConnectionFailed("cancel request timed out".to_owned())),
        };
        drop(tokio::time::timeout(Duration::from_millis(500), cancel_conn.disconnect()).await);
        res
    }
}

fn is_mysql_system_schema(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "information_schema" | "mysql" | "performance_schema" | "sys"
    )
}

fn is_mysql_permission_error(err: &MysqlError) -> bool {
    matches!(err, MysqlError::Server(se) if matches!(se.code, 1044 | 1142 | 1227))
}

fn is_prefer_tls_fallback_error(err: &MysqlError) -> bool {
    match err {
        MysqlError::Driver(
            DriverError::NoClientSslFlagFromServer
            | DriverError::ConnectionClosed
            | DriverError::UnexpectedPacket { .. }
            | DriverError::PacketOutOfOrder,
        ) => true,
        MysqlError::Io(IoError::Tls(_)) => true,
        MysqlError::Io(IoError::Io(io_err)) => !matches!(
            io_err.kind(),
            io::ErrorKind::ConnectionRefused
                | io::ErrorKind::TimedOut
                | io::ErrorKind::AddrNotAvailable
                | io::ErrorKind::NotFound
        ),
        _ => false,
    }
}

async fn connect_conn_with_fallback(
    profile: &ConnectionProfile,
    password: Option<&str>,
    target_db: Option<&str>,
) -> Result<(mysql_async::Conn, Opts), DbError> {
    let host = profile.host.trim();
    let initial_ssl = if host.starts_with('/') {
        None
    } else {
        build_mysql_ssl_opts(&profile.ssl)
    };

    let opts = build_mysql_opts(profile, password, target_db, initial_ssl);
    match tokio::time::timeout(CONNECT_TIMEOUT, mysql_async::Conn::new(opts.clone())).await {
        Ok(Ok(conn)) => Ok((conn, opts)),
        Ok(Err(err))
            if profile.ssl.mode == SslMode::Prefer && !host.starts_with('/') && is_prefer_tls_fallback_error(&err) =>
        {
            let plain_opts = build_mysql_opts(profile, password, target_db, None);
            match tokio::time::timeout(CONNECT_TIMEOUT, mysql_async::Conn::new(plain_opts.clone())).await {
                Ok(Ok(conn)) => Ok((conn, plain_opts)),
                Ok(Err(plain_err)) => Err(map_mysql_err(plain_err)),
                Err(_) => Err(DbError::ConnectionFailed("connection timed out".to_owned())),
            }
        }
        Ok(Err(err)) => Err(map_mysql_err(err)),
        Err(_) => Err(DbError::ConnectionFailed("connection timed out".to_owned())),
    }
}

/// Builds `mysql_async::SslOpts` from [`SslConfig`], returning `None` when TLS is disabled.
pub fn build_mysql_ssl_opts(ssl: &SslConfig) -> Option<SslOpts> {
    ensure_crypto_provider();
    let mut opts = match ssl.mode {
        SslMode::Disable => return None,
        SslMode::Prefer | SslMode::Require => SslOpts::default()
            .with_danger_accept_invalid_certs(true)
            .with_danger_skip_domain_validation(true),
        SslMode::VerifyCa => {
            let mut base = SslOpts::default()
                .with_danger_accept_invalid_certs(false)
                .with_danger_skip_domain_validation(true);
            if let Some(ca) = &ssl.ca_cert_path {
                base = base
                    .with_root_certs(vec![ca.clone().into()])
                    .with_disable_built_in_roots(true);
            }
            base
        }
        SslMode::VerifyFull => {
            let mut base = SslOpts::default()
                .with_danger_accept_invalid_certs(false)
                .with_danger_skip_domain_validation(false);
            if let Some(ca) = &ssl.ca_cert_path {
                base = base
                    .with_root_certs(vec![ca.clone().into()])
                    .with_disable_built_in_roots(true);
            }
            base
        }
    };

    if let (Some(cert), Some(key)) = (&ssl.client_cert_path, &ssl.client_key_path) {
        opts = opts.with_client_identity(Some(ClientIdentity::new(cert.clone().into(), key.clone().into())));
    }

    Some(opts)
}

/// Builds `mysql_async::Opts` from a [`ConnectionProfile`], optional password, target database, and resolved SSL opts.
pub fn build_mysql_opts(
    profile: &ConnectionProfile,
    password: Option<&str>,
    target_db: Option<&str>,
    ssl_opts: Option<SslOpts>,
) -> Opts {
    let host = profile.host.trim();
    let mut builder = OptsBuilder::default();
    if host.starts_with('/') {
        builder = builder.socket(Some(host));
    } else {
        builder = builder
            .ip_or_hostname(host)
            .tcp_port(profile.port)
            .prefer_socket(false)
            .tcp_keepalive(Some(Duration::from_secs(60)));
    }

    builder = builder.user(Some(profile.user.trim()));
    if profile.password_mode != PasswordMode::None {
        builder = builder.pass(password);
    }
    builder = builder.db_name(target_db);
    if profile.safety == SafetyPolicy::ReadOnly {
        builder = builder.setup(vec!["SET SESSION TRANSACTION READ ONLY".to_owned()]);
    }
    if let Some(ssl) = ssl_opts {
        builder = builder.ssl_opts(ssl);
    }
    Opts::from(builder)
}

/// Maps a `mysql_async::Error` from an established session into our typed [`DbError`].
pub fn map_mysql_session_err(err: MysqlError) -> DbError {
    match &err {
        MysqlError::Driver(DriverError::ConnectionClosed | DriverError::PoolDisconnected) => DbError::Disconnected,
        MysqlError::Server(se) if matches!(se.code, 1927 | 2006 | 2013 | 4031) => DbError::Disconnected,
        MysqlError::Io(IoError::Io(io_err))
            if matches!(
                io_err.kind(),
                io::ErrorKind::BrokenPipe
                    | io::ErrorKind::ConnectionReset
                    | io::ErrorKind::ConnectionAborted
                    | io::ErrorKind::UnexpectedEof
                    | io::ErrorKind::NotConnected
            ) =>
        {
            DbError::Disconnected
        }
        _ => map_mysql_err(err),
    }
}

/// Maps a `mysql_async::Error` into our typed [`DbError`].
pub fn map_mysql_err(err: MysqlError) -> DbError {
    match err {
        MysqlError::Server(se) => {
            if se.code == 1317 {
                return DbError::Cancelled;
            }
            if matches!(se.code, 1045 | 1044 | 1698) {
                return DbError::AuthFailed(se.message);
            }
            DbError::QueryFailed {
                message: se.message,
                code: Some(se.code.to_string()),
                detail: Some(format!("SQLSTATE {}", se.state)),
                hint: None,
            }
        }
        MysqlError::Driver(DriverError::ConnectionClosed | DriverError::PoolDisconnected) => DbError::Disconnected,
        MysqlError::Io(IoError::Tls(tls_err)) => DbError::TlsConfig(tls_err.to_string()),
        other => DbError::ConnectionFailed(other.to_string()),
    }
}

/// Extracts a concise MySQL or MariaDB version string (e.g. `"MySQL 8.4.0"` or `"MariaDB 11.4.2"`).
pub fn concise_mysql_version(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return "MySQL".to_owned();
    }
    let token = trimmed.split_whitespace().next().unwrap_or(trimmed);
    let lower = trimmed.to_ascii_lowercase();
    if lower.contains("mariadb") {
        let stripped = token.strip_prefix("5.5.5-").unwrap_or(token);
        let ver = stripped.split('-').next().unwrap_or(stripped).trim();
        if ver.is_empty() {
            "MariaDB".to_owned()
        } else {
            format!("MariaDB {ver}")
        }
    } else {
        let ver = token.split('-').next().unwrap_or(token).trim();
        if ver.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            format!("MySQL {ver}")
        } else {
            trimmed.to_owned()
        }
    }
}

type MysqlFkRowTuple = (
    Option<String>,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<i64>,
);

struct MysqlFkGroup {
    target_schema: String,
    target_table: Option<String>,
    table_consistent: bool,
    entries: Vec<(Option<i64>, Option<String>, Option<String>)>,
}

fn assemble_mysql_foreign_keys(default_schema: &str, rows: Vec<MysqlFkRowTuple>) -> Vec<ForeignKeyRef> {
    let mut groups: BTreeMap<String, MysqlFkGroup> = BTreeMap::new();
    for (src_col, constraint_name, target_schema, target_table, dst_col, ordinal_pos) in rows {
        let resolved_schema = target_schema
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| default_schema.to_owned());
        let entry = groups.entry(constraint_name).or_insert_with(|| MysqlFkGroup {
            target_schema: resolved_schema,
            target_table: target_table.clone(),
            table_consistent: true,
            entries: Vec::new(),
        });
        if entry.target_table != target_table {
            entry.table_consistent = false;
        }
        entry.entries.push((ordinal_pos, src_col, dst_col));
    }

    groups
        .into_iter()
        .filter_map(|(constraint_name, mut group)| {
            group.entries.sort_by_key(|(ord, _, _)| ord.unwrap_or(i64::MAX));
            let source_columns: Vec<String> = group
                .entries
                .iter()
                .filter_map(|(_, src, _)| src.as_deref().filter(|s| !s.is_empty()).map(str::to_owned))
                .collect();
            if source_columns.is_empty() {
                return None;
            }
            let target_table = group.target_table.unwrap_or_default();
            let contiguous_ordinals = !group.entries.is_empty()
                && group
                    .entries
                    .iter()
                    .enumerate()
                    .all(|(idx, (ord, _, _))| *ord == Some(idx as i64 + 1));
            let all_resolved = group.table_consistent
                && !target_table.trim().is_empty()
                && contiguous_ordinals
                && group.entries.iter().all(|(_, src, dst)| {
                    src.as_deref().is_some_and(|s| !s.is_empty()) && dst.as_deref().is_some_and(|d| !d.is_empty())
                });
            let target_columns = if all_resolved {
                group.entries.into_iter().filter_map(|(_, _, dst)| dst).collect()
            } else {
                Vec::new()
            };
            Some(ForeignKeyRef {
                constraint_name,
                target_schema: group.target_schema,
                target_table,
                source_columns,
                target_columns,
            })
        })
        .collect()
}

struct MysqlIndexCol {
    name: Option<String>,
    is_prefix: bool,
}

struct MysqlIndexMeta {
    is_unique: bool,
    cols: Vec<MysqlIndexCol>,
}

fn select_mysql_stable_order_keys(
    columns: &[ColumnMeta],
    index_cols: &HashMap<String, MysqlIndexMeta>,
) -> Vec<StableOrderKey> {
    if let Some(pk_meta) = index_cols.get("PRIMARY")
        && !pk_meta.cols.is_empty()
        && pk_meta.cols.iter().all(|col| {
            !col.is_prefix
                && col
                    .name
                    .as_deref()
                    .is_some_and(|name| columns.iter().any(|c| c.name == name))
        })
    {
        return pk_meta
            .cols
            .iter()
            .filter_map(|col| col.name.clone())
            .map(|name| StableOrderKey::Column {
                name,
                nullable: false,
                collation: None,
            })
            .collect();
    }

    let mut unique_candidates: Vec<(&str, &[MysqlIndexCol])> = index_cols
        .iter()
        .filter_map(|(name, meta)| {
            if name == "PRIMARY" || !meta.is_unique || meta.cols.is_empty() {
                return None;
            }
            let all_non_null_full = meta.cols.iter().all(|col| {
                !col.is_prefix
                    && col
                        .name
                        .as_deref()
                        .is_some_and(|col_name| columns.iter().any(|c| c.name == col_name && !c.nullable))
            });
            all_non_null_full.then_some((name.as_str(), meta.cols.as_slice()))
        })
        .collect();
    unique_candidates.sort_by(|a, b| a.1.len().cmp(&b.1.len()).then_with(|| a.0.cmp(b.0)));

    if let Some((_, cols)) = unique_candidates.first() {
        return cols
            .iter()
            .filter_map(|col| col.name.clone())
            .map(|name| StableOrderKey::Column {
                name,
                nullable: false,
                collation: None,
            })
            .collect();
    }

    Vec::new()
}

/// Builds a paginated MySQL `SELECT` statement with backtick quoting, deterministic ordering, and `NULLS LAST` emulation.
pub fn build_mysql_page_sql(
    schema: &str,
    table: &str,
    columns: &[ColumnMeta],
    req: &PageRequest,
    stable_order: &[StableOrderKey],
    fetch_limit: i64,
    offset: i64,
) -> String {
    let projection = if columns.is_empty() {
        "*".to_owned()
    } else {
        columns
            .iter()
            .map(|c| DbEngine::Mysql.quote_ident(&c.name))
            .collect::<Vec<_>>()
            .join(", ")
    };

    let fq_table = DbEngine::Mysql.qualify_ident(schema, table);
    let mut sql = format!("SELECT {projection} FROM {fq_table}");

    if let Some(where_clause) = &req.filter_where {
        let trimmed = where_clause.trim();
        if !trimmed.is_empty() {
            sql.push_str(&format!(" WHERE ({trimmed})"));
        }
    }

    let mut order_terms = Vec::new();
    if let Some(sort) = &req.sort {
        let quoted_col = DbEngine::Mysql.quote_ident(&sort.column);
        let dir = if sort.ascending { "ASC" } else { "DESC" };
        order_terms.push(format!("({quoted_col} IS NULL) ASC, {quoted_col} {dir}"));
    }
    for key in stable_order {
        match key {
            StableOrderKey::Column { name, nullable, .. } => {
                if req.sort.as_ref().is_some_and(|s| s.column == *name) {
                    continue;
                }
                let quoted_col = DbEngine::Mysql.quote_ident(name);
                if *nullable {
                    order_terms.push(format!("({quoted_col} IS NULL) ASC, {quoted_col} ASC"));
                } else {
                    order_terms.push(format!("{quoted_col} ASC"));
                }
            }
            StableOrderKey::System(ident) => {
                order_terms.push(format!("{ident} ASC"));
            }
        }
    }
    if !order_terms.is_empty() {
        sql.push_str(&format!(" ORDER BY {}", order_terms.join(", ")));
    }

    sql.push_str(&format!(" LIMIT {} OFFSET {}", fetch_limit.max(1), offset.max(0)));
    sql
}

/// Synthesizes a readable MySQL `CREATE TABLE` DDL statement when `SHOW CREATE TABLE` is unavailable.
pub fn synthesize_mysql_table_ddl(
    schema: &str,
    table: &str,
    columns: &[ColumnMeta],
    foreign_keys: &[ForeignKeyRef],
    indexes: &[IndexInfo],
) -> String {
    let fq_table = DbEngine::Mysql.qualify_ident(schema, table);
    let mut out = format!("CREATE TABLE {fq_table} (\n");
    let mut clauses: Vec<String> = Vec::with_capacity(columns.len() + 4);

    for col in columns {
        let mut line = format!("    {} {}", DbEngine::Mysql.quote_ident(&col.name), col.data_type);
        if !col.nullable {
            line.push_str(" NOT NULL");
        }
        if let Some(def) = &col.default_expr {
            if def.eq_ignore_ascii_case("auto_increment") {
                line.push_str(" AUTO_INCREMENT");
            } else {
                line.push_str(&format!(" DEFAULT {def}"));
            }
        }
        clauses.push(line);
    }

    let pk_cols: Vec<String> = columns
        .iter()
        .filter(|c| c.is_primary_key)
        .map(|c| DbEngine::Mysql.quote_ident(&c.name))
        .collect();
    if !pk_cols.is_empty() {
        clauses.push(format!("    PRIMARY KEY ({})", pk_cols.join(", ")));
    }

    for fk in crate::engine::collect_foreign_keys(foreign_keys, columns) {
        if !fk.source_columns.is_empty() {
            let target_ref = DbEngine::Mysql.qualify_ident(&fk.target_schema, &fk.target_table);
            let src_list = fk
                .source_columns
                .iter()
                .map(|c| DbEngine::Mysql.quote_ident(c))
                .collect::<Vec<_>>()
                .join(", ");
            if fk.is_resolvable() {
                let dst_list = fk
                    .target_columns
                    .iter()
                    .map(|c| DbEngine::Mysql.quote_ident(c))
                    .collect::<Vec<_>>()
                    .join(", ");
                clauses.push(format!(
                    "    CONSTRAINT {} FOREIGN KEY ({src_list}) REFERENCES {target_ref} ({dst_list})",
                    DbEngine::Mysql.quote_ident(&fk.constraint_name),
                ));
            } else {
                clauses.push(format!(
                    "    CONSTRAINT {} FOREIGN KEY ({src_list}) REFERENCES {target_ref}",
                    DbEngine::Mysql.quote_ident(&fk.constraint_name),
                ));
            }
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

fn column_meta_from_wire_column(col: &Column) -> ColumnMeta {
    let col_type = col.column_type();
    let is_binary_charset = col.character_set() == 63;
    let (data_type, udt_name) = match col_type {
        ColumnType::MYSQL_TYPE_TINY => {
            if col.column_length() == 1 {
                ("tinyint(1)", "tinyint(1)")
            } else {
                ("tinyint", "tinyint")
            }
        }
        ColumnType::MYSQL_TYPE_SHORT => ("smallint", "smallint"),
        ColumnType::MYSQL_TYPE_INT24 => ("mediumint", "mediumint"),
        ColumnType::MYSQL_TYPE_LONG => ("int", "int"),
        ColumnType::MYSQL_TYPE_LONGLONG => ("bigint", "bigint"),
        ColumnType::MYSQL_TYPE_FLOAT => ("float", "float"),
        ColumnType::MYSQL_TYPE_DOUBLE => ("double", "double"),
        ColumnType::MYSQL_TYPE_DECIMAL | ColumnType::MYSQL_TYPE_NEWDECIMAL => ("decimal", "decimal"),
        ColumnType::MYSQL_TYPE_DATE | ColumnType::MYSQL_TYPE_NEWDATE => ("date", "date"),
        ColumnType::MYSQL_TYPE_TIME | ColumnType::MYSQL_TYPE_TIME2 => ("time", "time"),
        ColumnType::MYSQL_TYPE_DATETIME | ColumnType::MYSQL_TYPE_DATETIME2 => ("datetime", "datetime"),
        ColumnType::MYSQL_TYPE_TIMESTAMP | ColumnType::MYSQL_TYPE_TIMESTAMP2 => ("timestamp", "timestamp"),
        ColumnType::MYSQL_TYPE_YEAR => ("year", "year"),
        ColumnType::MYSQL_TYPE_BIT => {
            if col.column_length() == 1 {
                ("bit(1)", "bit")
            } else {
                ("bit", "bit")
            }
        }
        ColumnType::MYSQL_TYPE_JSON => ("json", "json"),
        ColumnType::MYSQL_TYPE_ENUM => ("enum", "enum"),
        ColumnType::MYSQL_TYPE_SET => ("set", "set"),
        ColumnType::MYSQL_TYPE_TINY_BLOB
        | ColumnType::MYSQL_TYPE_MEDIUM_BLOB
        | ColumnType::MYSQL_TYPE_LONG_BLOB
        | ColumnType::MYSQL_TYPE_BLOB => {
            if is_binary_charset {
                ("blob", "blob")
            } else {
                ("text", "text")
            }
        }
        ColumnType::MYSQL_TYPE_STRING => {
            if is_binary_charset {
                ("binary", "binary")
            } else {
                ("char", "char")
            }
        }
        ColumnType::MYSQL_TYPE_VARCHAR | ColumnType::MYSQL_TYPE_VAR_STRING => {
            if is_binary_charset {
                ("varbinary", "varbinary")
            } else {
                ("varchar", "varchar")
            }
        }
        ColumnType::MYSQL_TYPE_GEOMETRY => ("geometry", "geometry"),
        ColumnType::MYSQL_TYPE_NULL => ("null", "null"),
        _ => ("text", "text"),
    };

    let flags = col.flags();
    let is_primary_key = flags.contains(ColumnFlags::PRI_KEY_FLAG);
    let is_unique = !is_primary_key && flags.contains(ColumnFlags::UNIQUE_KEY_FLAG);

    ColumnMeta {
        name: col.name_str().into_owned(),
        data_type: data_type.to_owned(),
        udt_name: udt_name.to_owned(),
        nullable: !flags.contains(ColumnFlags::NOT_NULL_FLAG),
        default_expr: None,
        is_primary_key,
        is_unique,
        comment: None,
        foreign_key: None,
    }
}

/// Decodes a `mysql_async::Value` (from either text or binary protocol) into a typed [`CellValue`].
pub fn decode_mysql_cell(
    val: &Value,
    wire_col: Option<&Column>,
    meta: Option<&ColumnMeta>,
) -> CellValue {
    let is_bool_col = is_mysql_bool_column(wire_col, meta);

    match val {
        Value::NULL => CellValue::Null,
        Value::Int(i) => {
            if is_bool_col {
                CellValue::Bool(*i != 0)
            } else {
                CellValue::Int(*i)
            }
        }
        Value::UInt(u) => {
            if is_bool_col {
                CellValue::Bool(*u != 0)
            } else if let Ok(i) = i64::try_from(*u) {
                CellValue::Int(i)
            } else {
                CellValue::Numeric(u.to_string())
            }
        }
        Value::Float(f) => {
            let v = f64::from(*f);
            if v.is_finite() {
                CellValue::Float(v)
            } else {
                CellValue::Text(v.to_string())
            }
        }
        Value::Double(d) => {
            if d.is_finite() {
                CellValue::Float(*d)
            } else {
                CellValue::Text(d.to_string())
            }
        }
        Value::Date(y, m, d, h, min, s, us) => {
            let is_date_only = meta.is_some_and(|c| c.udt_name.eq_ignore_ascii_case("date"))
                || wire_col.is_some_and(|c| matches!(c.column_type(), ColumnType::MYSQL_TYPE_DATE | ColumnType::MYSQL_TYPE_NEWDATE));
            if is_date_only && *h == 0 && *min == 0 && *s == 0 && *us == 0 {
                CellValue::Date(format!("{y:04}-{m:02}-{d:02}"))
            } else if *us == 0 {
                let raw = format!("{y:04}-{m:02}-{d:02} {h:02}:{min:02}:{s:02}");
                CellValue::Timestamp { raw, utc: None }
            } else {
                let raw = format!("{y:04}-{m:02}-{d:02} {h:02}:{min:02}:{s:02}.{us:06}");
                CellValue::Timestamp { raw, utc: None }
            }
        }
        Value::Time(neg, days, h, m, s, us) => {
            let sign = if *neg { "-" } else { "" };
            let total_hours = days.saturating_mul(24).saturating_add(u32::from(*h));
            let formatted = if *us == 0 {
                format!("{sign}{total_hours:02}:{m:02}:{s:02}")
            } else {
                format!("{sign}{total_hours:02}:{m:02}:{s:02}.{us:06}")
            };
            CellValue::Time(formatted)
        }
        Value::Bytes(bytes) => decode_mysql_bytes_cell(bytes, wire_col, meta, is_bool_col),
    }
}

fn is_mysql_bool_column(wire_col: Option<&Column>, meta: Option<&ColumnMeta>) -> bool {
    if let Some(m) = meta {
        let dtype = m.data_type.trim().to_ascii_lowercase();
        let udt = m.udt_name.trim().to_ascii_lowercase();
        if dtype.starts_with("tinyint(1)")
            || dtype == "bit(1)"
            || matches!(dtype.as_str(), "bool" | "boolean")
            || udt.starts_with("tinyint(1)")
            || matches!(udt.as_str(), "bool" | "boolean")
        {
            return true;
        }
    }
    if let Some(c) = wire_col
        && matches!(c.column_type(), ColumnType::MYSQL_TYPE_TINY | ColumnType::MYSQL_TYPE_BIT)
        && c.column_length() == 1
    {
        return true;
    }
    false
}

fn decode_mysql_bytes_cell(
    bytes: &[u8],
    wire_col: Option<&Column>,
    meta: Option<&ColumnMeta>,
    is_bool_col: bool,
) -> CellValue {
    let col_type = wire_col.map(Column::column_type);
    let meta_udt = meta.map(|m| m.udt_name.trim().to_ascii_lowercase());
    let meta_dtype = meta.map(|m| m.data_type.trim().to_ascii_lowercase());

    if is_bool_col {
        return match bytes {
            [0] | b"0" => CellValue::Bool(false),
            [1] | b"1" => CellValue::Bool(true),
            _ => match std::str::from_utf8(bytes) {
                Ok(s) if s.eq_ignore_ascii_case("true") => CellValue::Bool(true),
                Ok(s) if s.eq_ignore_ascii_case("false") => CellValue::Bool(false),
                Ok(s) => CellValue::Text(s.to_owned()),
                Err(_) => CellValue::Bytes(bytes.to_vec()),
            },
        };
    }

    if col_type == Some(ColumnType::MYSQL_TYPE_BIT) || meta_udt.as_deref() == Some("bit") {
        return CellValue::Bytes(bytes.to_vec());
    }

    if col_type == Some(ColumnType::MYSQL_TYPE_JSON) || meta_udt.as_deref() == Some("json") {
        return match std::str::from_utf8(bytes) {
            Ok(s) => CellValue::Json(s.to_owned()),
            Err(_) => CellValue::Bytes(bytes.to_vec()),
        };
    }

    let is_binary_meta = meta_udt.as_deref().is_some_and(|u| {
        matches!(
            u,
            "binary" | "varbinary" | "blob" | "tinyblob" | "mediumblob" | "longblob" | "geometry"
        )
    });
    let is_binary_wire = wire_col.is_some_and(|c| {
        c.character_set() == 63
            && matches!(
                c.column_type(),
                ColumnType::MYSQL_TYPE_BLOB
                    | ColumnType::MYSQL_TYPE_TINY_BLOB
                    | ColumnType::MYSQL_TYPE_MEDIUM_BLOB
                    | ColumnType::MYSQL_TYPE_LONG_BLOB
                    | ColumnType::MYSQL_TYPE_STRING
                    | ColumnType::MYSQL_TYPE_VAR_STRING
                    | ColumnType::MYSQL_TYPE_VARCHAR
                    | ColumnType::MYSQL_TYPE_GEOMETRY
            )
    });

    if is_binary_meta || is_binary_wire || col_type == Some(ColumnType::MYSQL_TYPE_GEOMETRY) {
        return CellValue::Bytes(bytes.to_vec());
    }

    let Ok(s) = std::str::from_utf8(bytes) else {
        return CellValue::Bytes(bytes.to_vec());
    };

    if matches!(
        col_type,
        Some(
            ColumnType::MYSQL_TYPE_TINY
                | ColumnType::MYSQL_TYPE_SHORT
                | ColumnType::MYSQL_TYPE_INT24
                | ColumnType::MYSQL_TYPE_LONG
                | ColumnType::MYSQL_TYPE_LONGLONG
                | ColumnType::MYSQL_TYPE_YEAR
        )
    ) || meta_udt.as_deref().is_some_and(|u| {
        matches!(
            u,
            "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint" | "year"
        )
    }) {
        return match s.trim().parse::<i64>() {
            Ok(i) => CellValue::Int(i),
            Err(_) => CellValue::Numeric(s.to_owned()),
        };
    }

    if matches!(
        col_type,
        Some(ColumnType::MYSQL_TYPE_FLOAT | ColumnType::MYSQL_TYPE_DOUBLE)
    ) || meta_udt
        .as_deref()
        .is_some_and(|u| matches!(u, "float" | "double" | "real" | "double precision"))
    {
        return match s.trim().parse::<f64>() {
            Ok(v) if v.is_finite() => CellValue::Float(v),
            _ => CellValue::Text(s.to_owned()),
        };
    }

    if matches!(
        col_type,
        Some(ColumnType::MYSQL_TYPE_DECIMAL | ColumnType::MYSQL_TYPE_NEWDECIMAL)
    ) || meta_udt
        .as_deref()
        .is_some_and(|u| matches!(u, "decimal" | "numeric"))
    {
        return CellValue::Numeric(s.to_owned());
    }

    if matches!(
        col_type,
        Some(ColumnType::MYSQL_TYPE_DATE | ColumnType::MYSQL_TYPE_NEWDATE)
    ) || meta_udt.as_deref() == Some("date")
    {
        return CellValue::Date(s.to_owned());
    }

    if matches!(
        col_type,
        Some(ColumnType::MYSQL_TYPE_TIME | ColumnType::MYSQL_TYPE_TIME2)
    ) || meta_udt.as_deref() == Some("time")
    {
        return CellValue::Time(s.to_owned());
    }

    if matches!(
        col_type,
        Some(
            ColumnType::MYSQL_TYPE_DATETIME
                | ColumnType::MYSQL_TYPE_DATETIME2
                | ColumnType::MYSQL_TYPE_TIMESTAMP
                | ColumnType::MYSQL_TYPE_TIMESTAMP2
        )
    ) || meta_udt
        .as_deref()
        .is_some_and(|u| matches!(u, "datetime" | "timestamp"))
    {
        return CellValue::Timestamp {
            utc: parse_mysql_timestamp(s),
            raw: s.to_owned(),
        };
    }

    if (meta_udt.as_deref() == Some("uuid") || (meta_dtype.as_deref() == Some("uuid") && s.len() == 36))
        && let Ok(u) = Uuid::parse_str(s.trim())
    {
        return CellValue::Uuid(u);
    }

    CellValue::Text(s.to_owned())
}

fn parse_mysql_timestamp(raw: &str) -> Option<DateTime<Utc>> {
    let trimmed = raw.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(trimmed) {
        return Some(dt.with_timezone(&Utc));
    }
    for fmt in [
        "%Y-%m-%d %H:%M:%S%.f%#z",
        "%Y-%m-%d %H:%M:%S%#z",
        "%Y-%m-%dT%H:%M:%S%.f%#z",
        "%Y-%m-%dT%H:%M:%S%#z",
    ] {
        if let Ok(dt) = DateTime::parse_from_str(trimmed, fmt) {
            return Some(dt.with_timezone(&Utc));
        }
    }
    None
}

fn stale_conflict_msg(op_kind: &str, row_label: &str, count: u64) -> String {
    if count == 0 {
        format!(
            "Staged {op_kind} ({row_label}) matched 0 rows: the row was modified or deleted since it was loaded. Transaction rolled back; review your staged changes and reload."
        )
    } else {
        format!(
            "Staged {op_kind} ({row_label}) matched {count} rows instead of 1. Transaction rolled back to prevent unintended changes."
        )
    }
}

pub(crate) fn classify_mysql_table_transaction_support(
    table_type: Option<&str>,
    engine: Option<&str>,
    transactions: Option<&str>,
) -> TableTransactionSupport {
    if let Some(tt) = table_type
        && tt.to_ascii_uppercase().contains("VIEW")
    {
        return TableTransactionSupport::Unsupported {
            engine: "VIEW".to_owned(),
        };
    }
    if let Some(tx) = transactions.map(str::trim).filter(|s| !s.is_empty()) {
        if tx.eq_ignore_ascii_case("YES") {
            return TableTransactionSupport::Supported;
        }
        if tx.eq_ignore_ascii_case("NO") {
            let eng = engine
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("unknown")
                .to_owned();
            return TableTransactionSupport::Unsupported { engine: eng };
        }
    }
    let Some(eng) = engine.map(str::trim).filter(|s| !s.is_empty()) else {
        return TableTransactionSupport::Unknown;
    };
    match eng.to_ascii_uppercase().as_str() {
        "INNODB" | "XTRADB" | "TOKUDB" | "ROCKSDB" | "MYROCKS" | "NDB" | "NDBCLUSTER" => {
            TableTransactionSupport::Supported
        }
        "MYISAM" | "MEMORY" | "HEAP" | "CSV" | "ARCHIVE" | "BLACKHOLE" | "MERGE" | "MRG_MYISAM" | "FEDERATED"
        | "ARIA" | "ISAM" => TableTransactionSupport::Unsupported {
            engine: eng.to_owned(),
        },
        _ => TableTransactionSupport::Unknown,
    }
}

impl MysqlSession {
    async fn fetch_mysql_table_meta(
        &self,
        conn: &mut mysql_async::Conn,
        schema: &str,
        table: &str,
    ) -> Result<(TableTransactionSupport, Option<i64>, Option<ExecutedSql>), DbError> {
        let meta_sql = "SELECT t.TABLE_TYPE, t.ENGINE, e.TRANSACTIONS, t.TABLE_ROWS \
                        FROM information_schema.TABLES t \
                        LEFT JOIN information_schema.ENGINES e ON UPPER(e.ENGINE) = UPPER(t.ENGINE) \
                        WHERE t.TABLE_SCHEMA = ? AND t.TABLE_NAME = ?";
        let start = Instant::now();
        let (row, used_sql): (Option<Row>, &str) = match conn.exec_first::<Row, _, _>(meta_sql, (schema, table)).await {
            Ok(r) => (r, meta_sql),
            Err(err) if is_mysql_permission_error(&err) => {
                let fallback_sql = "SELECT TABLE_TYPE, ENGINE, NULL, TABLE_ROWS \
                                    FROM information_schema.TABLES \
                                    WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?";
                match conn.exec_first::<Row, _, _>(fallback_sql, (schema, table)).await {
                    Ok(r) => (r, fallback_sql),
                    Err(err2) if is_mysql_permission_error(&err2) => {
                        return Ok((TableTransactionSupport::Unknown, None, None));
                    }
                    Err(err2) => return Err(self.map_session_err(err2)),
                }
            }
            Err(err) => return Err(self.map_session_err(err)),
        };
        let duration_ms = start.elapsed().as_millis() as u64;
        let log = ExecutedSql {
            sql: format!(
                "{used_sql} -- ? = '{}', ? = '{}'",
                schema.replace('\'', "''"),
                table.replace('\'', "''")
            ),
            duration_ms,
            error: None,
        };
        let Some(row) = row else {
            return Ok((TableTransactionSupport::Unknown, None, Some(log)));
        };
        let table_type = value_to_opt_string(row.as_ref(0).unwrap_or(&Value::NULL));
        let engine_name = value_to_opt_string(row.as_ref(1).unwrap_or(&Value::NULL));
        let tx_flag = value_to_opt_string(row.as_ref(2).unwrap_or(&Value::NULL));
        let table_rows = row.as_ref(3).and_then(value_to_opt_i64);
        let support = classify_mysql_table_transaction_support(
            table_type.as_deref(),
            engine_name.as_deref(),
            tx_flag.as_deref(),
        );
        Ok((support, table_rows, Some(log)))
    }
}

#[derive(Debug, PartialEq)]
pub(crate) enum MysqlMutationPlan {
    Update {
        row_label: String,
        lock_sql: String,
        lock_params: Vec<Value>,
        exec_sql: String,
        exec_params: Vec<Value>,
    },
    Insert {
        row_label: String,
        exec_sql: String,
        exec_params: Vec<Value>,
    },
    Delete {
        row_label: String,
        lock_sql: String,
        lock_params: Vec<Value>,
        exec_sql: String,
        exec_params: Vec<Value>,
    },
}

fn mysql_val_to_raw(val: &Value) -> MysqlRawValue {
    match val {
        Value::NULL => MysqlRawValue::Null,
        Value::Int(i) => MysqlRawValue::Int(*i),
        Value::UInt(u) => MysqlRawValue::UInt(*u),
        Value::Float(f) => MysqlRawValue::Float(*f),
        Value::Double(d) => MysqlRawValue::Double(*d),
        Value::Bytes(b) => MysqlRawValue::Bytes(b.clone()),
        Value::Date(year, month, day, hour, minute, second, micros) => MysqlRawValue::Date {
            year: *year,
            month: *month,
            day: *day,
            hour: *hour,
            minute: *minute,
            second: *second,
            micros: *micros,
        },
        Value::Time(neg, days, hour, minute, second, micros) => MysqlRawValue::Time {
            neg: *neg,
            days: *days,
            hour: *hour,
            minute: *minute,
            second: *second,
            micros: *micros,
        },
    }
}

fn ensure_mysql_table_transactional(
    support: TableTransactionSupport,
    schema: &str,
    table: &str,
) -> Result<(), DbError> {
    match support {
        TableTransactionSupport::Supported => Ok(()),
        TableTransactionSupport::Unsupported { ref engine } => Err(DbError::NonTransactionalTable {
            message: format!(
                "Table `{schema}`.`{table}` uses non-transactional storage engine '{engine}'; staged commits require a transactional engine (such as InnoDB)."
            ),
            support,
        }),
        TableTransactionSupport::Unknown => Err(DbError::NonTransactionalTable {
            support,
            message: format!(
                "Could not verify transactional storage engine support for `{schema}`.`{table}`; staged commits are blocked to avoid partial writes."
            ),
        }),
    }
}

fn mysql_assignment_value(col: &ColumnMeta, val: &StagedCellValue) -> (&'static str, Option<Value>) {
    match val {
        StagedCellValue::Null => ("NULL", None),
        StagedCellValue::DefaultExpr => ("DEFAULT", None),
        StagedCellValue::Literal(raw) => {
            let trimmed = raw.trim();
            match classify_column_type(col) {
                LogicalColType::Bool => match trimmed.to_ascii_lowercase().as_str() {
                    "true" | "t" | "1" | "yes" | "y" | "on" => ("?", Some(Value::Int(1))),
                    "false" | "f" | "0" | "no" | "n" | "off" => ("?", Some(Value::Int(0))),
                    _ => ("?", Some(Value::Bytes(trimmed.as_bytes().to_vec()))),
                },
                LogicalColType::Int => match trimmed.parse::<i64>() {
                    Ok(i) => ("?", Some(Value::Int(i))),
                    Err(_) => ("?", Some(Value::Bytes(trimmed.as_bytes().to_vec()))),
                },
                LogicalColType::Float => match trimmed.parse::<f64>() {
                    Ok(d) if d.is_finite() => ("?", Some(Value::Double(d))),
                    _ => ("?", Some(Value::Bytes(trimmed.as_bytes().to_vec()))),
                },
                LogicalColType::Bytes => {
                    let hex = strip_hex_prefix(trimmed);
                    match decode_hex(hex) {
                        Some(bytes) => ("?", Some(Value::Bytes(bytes))),
                        None => ("?", Some(Value::Bytes(raw.as_bytes().to_vec()))),
                    }
                }
                LogicalColType::Json => ("CAST(? AS JSON)", Some(Value::Bytes(raw.as_bytes().to_vec()))),
                LogicalColType::Numeric
                | LogicalColType::Date
                | LogicalColType::Time
                | LogicalColType::Timestamp
                | LogicalColType::TimestampTz
                | LogicalColType::Uuid => ("?", Some(Value::Bytes(trimmed.as_bytes().to_vec()))),
                LogicalColType::Text | LogicalColType::Array => {
                    ("?", Some(Value::Bytes(raw.as_bytes().to_vec())))
                }
            }
        }
    }
}

fn mysql_col_val_to_value(val: &StagedColumnValue) -> Value {
    if let Some(RawCellValue::Mysql(raw)) = &val.raw {
        return match raw {
            MysqlRawValue::Null => Value::NULL,
            MysqlRawValue::Int(i) => Value::Int(*i),
            MysqlRawValue::UInt(u) => Value::UInt(*u),
            MysqlRawValue::Float(f) => Value::Float(*f),
            MysqlRawValue::Double(d) => Value::Double(*d),
            MysqlRawValue::Date {
                year,
                month,
                day,
                hour,
                minute,
                second,
                micros,
            } => Value::Date(*year, *month, *day, *hour, *minute, *second, *micros),
            MysqlRawValue::Time {
                neg,
                days,
                hour,
                minute,
                second,
                micros,
            } => Value::Time(*neg, *days, *hour, *minute, *second, *micros),
            MysqlRawValue::Bytes(b) => {
                if classify_column_type(&val.column) == LogicalColType::Float
                    && let Ok(s) = std::str::from_utf8(b)
                {
                    let trimmed = s.trim();
                    if val.column.udt_name.eq_ignore_ascii_case("float")
                        && let Ok(f) = trimmed.parse::<f32>()
                        && f.is_finite()
                    {
                        return Value::Float(f);
                    }
                    if let Ok(d) = trimmed.parse::<f64>()
                        && d.is_finite()
                    {
                        return Value::Double(d);
                    }
                }
                Value::Bytes(b.clone())
            }
        };
    }
    match &val.display {
        CellValue::Null => Value::NULL,
        CellValue::Bool(b) => Value::Int(i64::from(*b)),
        CellValue::Int(i) => Value::Int(*i),
        CellValue::Float(d) => {
            if val.column.udt_name.eq_ignore_ascii_case("float") {
                Value::Float(*d as f32)
            } else {
                Value::Double(*d)
            }
        }
        CellValue::Bytes(b) => Value::Bytes(b.clone()),
        other => Value::Bytes(other.copy_text().into_bytes()),
    }
}

fn mysql_decimal_cast_type(data_type: &str) -> Option<String> {
    let lower = data_type.trim().to_ascii_lowercase();
    let rest = lower
        .strip_prefix("decimal")
        .or_else(|| lower.strip_prefix("numeric"))?
        .trim_start();
    let open = rest.strip_prefix('(')?;
    let close = open.find(')')?;
    let inner = &open[..close];
    let mut parts = inner.split(',').map(str::trim);
    let precision: u16 = parts.next()?.parse().ok()?;
    if !(1..=65).contains(&precision) {
        return None;
    }
    match parts.next() {
        Some(scale_str) if parts.next().is_none() => {
            let scale: u16 = scale_str.parse().ok()?;
            if scale <= precision.min(30) {
                Some(format!("DECIMAL({precision}, {scale})"))
            } else {
                None
            }
        }
        None => Some(format!("DECIMAL({precision})")),
        _ => None,
    }
}

fn push_mysql_value_predicate(
    clauses: &mut Vec<String>,
    params: &mut Vec<Value>,
    col: &ColumnMeta,
    bound: Value,
    is_pk: bool,
) {
    let q = DbEngine::Mysql.quote_ident(&col.name);
    if matches!(bound, Value::NULL) {
        clauses.push(format!("{q} IS NULL"));
        return;
    }
    if classify_column_type(col) == LogicalColType::Json {
        clauses.push(format!("{q} = CAST(? AS JSON)"));
        params.push(bound);
    } else if matches!(
        col.udt_name.trim().to_ascii_lowercase().as_str(),
        "decimal" | "numeric"
    ) && let Some(cast_ty) = mysql_decimal_cast_type(&col.data_type)
    {
        clauses.push(format!("{q} = CAST(? AS {cast_ty})"));
        params.push(bound);
    } else if is_mysql_text_col(col) {
        // Compare utf8mb4 bytes as BINARY so PAD SPACE trailing-space and case drift are detected.
        let exact_eq = format!(
            "CAST(CONVERT({q} USING utf8mb4) AS BINARY) = CAST(CONVERT(? USING utf8mb4) AS BINARY)"
        );
        if is_pk {
            clauses.push(format!("{q} = ? AND {exact_eq}"));
            params.push(bound.clone());
            params.push(bound);
        } else {
            clauses.push(exact_eq);
            params.push(bound);
        }
    } else {
        clauses.push(format!("{q} = ?"));
        params.push(bound);
    }
}

fn append_mysql_predicates(
    clauses: &mut Vec<String>,
    params: &mut Vec<Value>,
    pk_predicates: &[StagedColumnValue],
    concurrency_predicates: &[StagedColumnValue],
) -> Result<(), DbError> {
    if pk_predicates.is_empty() {
        return Err(DbError::InvalidInput(
            "table requires a primary key to update or delete existing rows safely".to_owned(),
        ));
    }
    for pk in pk_predicates {
        let bound = mysql_col_val_to_value(pk);
        if matches!(bound, Value::NULL) {
            return Err(DbError::InvalidInput(format!(
                "primary key column '{}' cannot be NULL",
                pk.column.name
            )));
        }
        push_mysql_value_predicate(clauses, params, &pk.column, bound, true);
    }
    for col in concurrency_predicates {
        let bound = mysql_col_val_to_value(col);
        push_mysql_value_predicate(clauses, params, &col.column, bound, false);
    }
    Ok(())
}

pub(crate) fn build_mysql_staged_mutation_plan(
    schema: &str,
    table: &str,
    mutation: &StagedMutation,
) -> Result<MysqlMutationPlan, DbError> {
    let fq_table = DbEngine::Mysql.qualify_ident(schema, table);
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
            let mut exec_params = Vec::new();
            for a in assignments {
                let (expr, maybe_param) = mysql_assignment_value(&a.column, &a.value);
                set_clauses.push(format!("{} = {expr}", DbEngine::Mysql.quote_ident(&a.column.name)));
                if let Some(p) = maybe_param {
                    exec_params.push(p);
                }
            }
            let mut where_clauses = Vec::new();
            let mut lock_params = Vec::new();
            append_mysql_predicates(
                &mut where_clauses,
                &mut lock_params,
                pk_predicates,
                concurrency_predicates,
            )?;
            exec_params.extend(lock_params.iter().cloned());
            let where_sql = where_clauses.join(" AND ");
            Ok(MysqlMutationPlan::Update {
                row_label: row_label.clone(),
                lock_sql: format!("SELECT COUNT(*) FROM {fq_table} WHERE {where_sql} FOR UPDATE"),
                lock_params,
                exec_sql: format!(
                    "UPDATE {fq_table} SET {} WHERE {where_sql};",
                    set_clauses.join(", ")
                ),
                exec_params,
            })
        }
        StagedMutation::Insert {
            row_label,
            assignments,
        } => {
            if assignments.is_empty() {
                return Ok(MysqlMutationPlan::Insert {
                    row_label: row_label.clone(),
                    exec_sql: format!("INSERT INTO {fq_table} () VALUES ();"),
                    exec_params: Vec::new(),
                });
            }
            let mut col_names = Vec::with_capacity(assignments.len());
            let mut val_exprs = Vec::with_capacity(assignments.len());
            let mut exec_params = Vec::new();
            for a in assignments {
                col_names.push(DbEngine::Mysql.quote_ident(&a.column.name));
                let (expr, maybe_param) = mysql_assignment_value(&a.column, &a.value);
                val_exprs.push(expr);
                if let Some(p) = maybe_param {
                    exec_params.push(p);
                }
            }
            Ok(MysqlMutationPlan::Insert {
                row_label: row_label.clone(),
                exec_sql: format!(
                    "INSERT INTO {fq_table} ({}) VALUES ({});",
                    col_names.join(", "),
                    val_exprs.join(", ")
                ),
                exec_params,
            })
        }
        StagedMutation::Delete {
            row_label,
            pk_predicates,
            concurrency_predicates,
        } => {
            let mut where_clauses = Vec::new();
            let mut lock_params = Vec::new();
            append_mysql_predicates(
                &mut where_clauses,
                &mut lock_params,
                pk_predicates,
                concurrency_predicates,
            )?;
            let where_sql = where_clauses.join(" AND ");
            let exec_params = lock_params.clone();
            Ok(MysqlMutationPlan::Delete {
                row_label: row_label.clone(),
                lock_sql: format!("SELECT COUNT(*) FROM {fq_table} WHERE {where_sql} FOR UPDATE"),
                lock_params,
                exec_sql: format!("DELETE FROM {fq_table} WHERE {where_sql};"),
                exec_params,
            })
        }
    }
}

fn value_to_string(val: &Value) -> String {
    match val {
        Value::NULL => String::new(),
        Value::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Double(d) => d.to_string(),
        Value::Date(y, m, d, 0, 0, 0, 0) => format!("{y:04}-{m:02}-{d:02}"),
        Value::Date(y, m, d, h, min, s, 0) => format!("{y:04}-{m:02}-{d:02} {h:02}:{min:02}:{s:02}"),
        Value::Date(y, m, d, h, min, s, us) => format!("{y:04}-{m:02}-{d:02} {h:02}:{min:02}:{s:02}.{us:06}"),
        Value::Time(neg, days, h, m, s, us) => {
            let sign = if *neg { "-" } else { "" };
            let total_h = days.saturating_mul(24).saturating_add(u32::from(*h));
            if *us == 0 {
                format!("{sign}{total_h:02}:{m:02}:{s:02}")
            } else {
                format!("{sign}{total_h:02}:{m:02}:{s:02}.{us:06}")
            }
        }
    }
}

fn value_to_opt_string(val: &Value) -> Option<String> {
    match val {
        Value::NULL => None,
        other => Some(value_to_string(other)),
    }
}

fn value_to_opt_i64(val: &Value) -> Option<i64> {
    match val {
        Value::NULL => None,
        Value::Int(i) => Some(*i),
        Value::UInt(u) => i64::try_from(*u).ok(),
        Value::Bytes(b) => std::str::from_utf8(b).ok().and_then(|s| s.trim().parse::<i64>().ok()),
        Value::Float(f) => Some(*f as i64),
        Value::Double(d) => Some(*d as i64),
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::path::PathBuf;

    use mysql_async::ServerError;

    use super::*;

    fn test_col(name: &str, data_type: &str, udt_name: &str) -> ColumnMeta {
        ColumnMeta {
            name: name.to_owned(),
            data_type: data_type.to_owned(),
            udt_name: udt_name.to_owned(),
            nullable: true,
            default_expr: None,
            is_primary_key: false,
            is_unique: false,
            comment: None,
            foreign_key: None,
        }
    }

    #[test]
    fn parses_concise_mysql_and_mariadb_versions() {
        assert_eq!(concise_mysql_version("8.4.0"), "MySQL 8.4.0");
        assert_eq!(concise_mysql_version("8.0.36-0ubuntu0.22.04.1"), "MySQL 8.0.36");
        assert_eq!(concise_mysql_version("9.1.0-Homebrew"), "MySQL 9.1.0");
        assert_eq!(concise_mysql_version("11.4.2-MariaDB"), "MariaDB 11.4.2");
        assert_eq!(
            concise_mysql_version("11.4.2-MariaDB-ubu2404"),
            "MariaDB 11.4.2"
        );
        assert_eq!(
            concise_mysql_version("5.5.5-10.11.6-MariaDB-1:10.11.6+maria~ubu2204"),
            "MariaDB 10.11.6"
        );
    }

    #[test]
    fn decodes_all_mysql_value_types_for_text_and_binary_protocols() {
        assert_eq!(decode_mysql_cell(&Value::NULL, None, None), CellValue::Null);

        // TINYINT(1) / BIT(1) / BOOLEAN
        let bool_wire = Column::new(ColumnType::MYSQL_TYPE_TINY).with_column_length(1);
        let bool_meta = test_col("enrolled", "tinyint(1)", "tinyint");
        assert_eq!(
            decode_mysql_cell(&Value::Bytes(b"1".to_vec()), Some(&bool_wire), Some(&bool_meta)),
            CellValue::Bool(true)
        );
        assert_eq!(
            decode_mysql_cell(&Value::Bytes(b"0".to_vec()), Some(&bool_wire), Some(&bool_meta)),
            CellValue::Bool(false)
        );
        assert_eq!(
            decode_mysql_cell(&Value::Int(1), Some(&bool_wire), None),
            CellValue::Bool(true)
        );

        let bit1_wire = Column::new(ColumnType::MYSQL_TYPE_BIT).with_column_length(1);
        assert_eq!(
            decode_mysql_cell(&Value::Bytes(vec![1]), Some(&bit1_wire), None),
            CellValue::Bool(true)
        );
        assert_eq!(
            decode_mysql_cell(&Value::Bytes(vec![0]), Some(&bit1_wire), None),
            CellValue::Bool(false)
        );

        // Regular integers (TINYINT(4), INT, BIGINT, BIGINT UNSIGNED)
        let tiny_wire = Column::new(ColumnType::MYSQL_TYPE_TINY).with_column_length(4);
        assert_eq!(
            decode_mysql_cell(&Value::Bytes(b"42".to_vec()), Some(&tiny_wire), None),
            CellValue::Int(42)
        );
        let big_wire = Column::new(ColumnType::MYSQL_TYPE_LONGLONG);
        assert_eq!(
            decode_mysql_cell(&Value::Bytes(b"-9000".to_vec()), Some(&big_wire), None),
            CellValue::Int(-9000)
        );
        assert_eq!(
            decode_mysql_cell(&Value::UInt(u64::MAX), Some(&big_wire), None),
            CellValue::Numeric(u64::MAX.to_string())
        );

        // Floats & Decimals
        let dbl_wire = Column::new(ColumnType::MYSQL_TYPE_DOUBLE);
        assert_eq!(
            decode_mysql_cell(&Value::Bytes(b"3.75".to_vec()), Some(&dbl_wire), None),
            CellValue::Float(3.75)
        );
        assert_eq!(
            decode_mysql_cell(&Value::Double(2.5), Some(&dbl_wire), None),
            CellValue::Float(2.5)
        );
        let dec_wire = Column::new(ColumnType::MYSQL_TYPE_NEWDECIMAL);
        assert_eq!(
            decode_mysql_cell(&Value::Bytes(b"3.9500".to_vec()), Some(&dec_wire), None),
            CellValue::Numeric("3.9500".to_owned())
        );

        // Date, Time, Datetime, Timestamp
        let date_wire = Column::new(ColumnType::MYSQL_TYPE_DATE);
        assert_eq!(
            decode_mysql_cell(&Value::Bytes(b"2026-11-19".to_vec()), Some(&date_wire), None),
            CellValue::Date("2026-11-19".to_owned())
        );
        assert_eq!(
            decode_mysql_cell(&Value::Date(2026, 11, 19, 0, 0, 0, 0), Some(&date_wire), None),
            CellValue::Date("2026-11-19".to_owned())
        );

        let time_wire = Column::new(ColumnType::MYSQL_TYPE_TIME);
        assert_eq!(
            decode_mysql_cell(&Value::Bytes(b"18:09:11".to_vec()), Some(&time_wire), None),
            CellValue::Time("18:09:11".to_owned())
        );
        assert_eq!(
            decode_mysql_cell(&Value::Time(false, 1, 2, 3, 4, 0), Some(&time_wire), None),
            CellValue::Time("26:03:04".to_owned())
        );

        let dt_wire = Column::new(ColumnType::MYSQL_TYPE_DATETIME);
        assert_eq!(
            decode_mysql_cell(&Value::Bytes(b"2026-11-19 18:09:11.432".to_vec()), Some(&dt_wire), None),
            CellValue::Timestamp {
                raw: "2026-11-19 18:09:11.432".to_owned(),
                utc: None,
            }
        );

        // JSON
        let json_wire = Column::new(ColumnType::MYSQL_TYPE_JSON);
        assert_eq!(
            decode_mysql_cell(&Value::Bytes(br#"{"campus":"Greendale"}"#.to_vec()), Some(&json_wire), None),
            CellValue::Json(r#"{"campus":"Greendale"}"#.to_owned())
        );

        // TEXT vs BLOB (charset 63 = binary)
        let text_wire = Column::new(ColumnType::MYSQL_TYPE_BLOB).with_character_set(45);
        assert_eq!(
            decode_mysql_cell(&Value::Bytes(b"Troy Barnes".to_vec()), Some(&text_wire), None),
            CellValue::Text("Troy Barnes".to_owned())
        );
        assert_eq!(
            decode_mysql_cell(&Value::Bytes(Vec::new()), Some(&text_wire), None),
            CellValue::Text(String::new())
        );

        let blob_wire = Column::new(ColumnType::MYSQL_TYPE_BLOB).with_character_set(63);
        assert_eq!(
            decode_mysql_cell(&Value::Bytes(vec![0xde, 0xad, 0xbe, 0xef]), Some(&blob_wire), None),
            CellValue::Bytes(vec![0xde, 0xad, 0xbe, 0xef])
        );
    }

    #[test]
    fn builds_mysql_page_sql_with_nulls_last_emulation() {
        let cols = vec![
            test_col("student_id", "int", "int"),
            test_col("full`name", "varchar(120)", "varchar"),
        ];
        let req = PageRequest {
            schema: "greendale".to_owned(),
            table: "students".to_owned(),
            filter_where: Some("enrolled = 1".to_owned()),
            sort: Some(crate::engine::SortSpec {
                column: "full`name".to_owned(),
                ascending: false,
            }),
            limit: 50,
            offset: 100,
        };
        let sql = build_mysql_page_sql(
            "greendale",
            "students",
            &cols,
            &req,
            &[StableOrderKey::Column {
                name: "student_id".to_owned(),
                nullable: false,
                collation: None,
            }],
            51,
            100,
        );
        assert_eq!(
            sql,
            "SELECT `student_id`, `full``name` FROM `greendale`.`students` \
             WHERE (enrolled = 1) ORDER BY (`full``name` IS NULL) ASC, `full``name` DESC, `student_id` ASC LIMIT 51 OFFSET 100"
        );
    }

    #[test]
    fn maps_mysql_error_codes_correctly() {
        let cancelled = map_mysql_err(MysqlError::Server(ServerError {
            code: 1317,
            message: "Query execution was interrupted".to_owned(),
            state: "70100".to_owned(),
        }));
        assert_eq!(cancelled, DbError::Cancelled);

        for code in [1045, 1044, 1698] {
            let auth = map_mysql_err(MysqlError::Server(ServerError {
                code,
                message: "Access denied for user 'abed'@'localhost'".to_owned(),
                state: "28000".to_owned(),
            }));
            assert!(auth.is_auth_failure(), "code {code} should map to AuthFailed");
        }

        let syntax = map_mysql_err(MysqlError::Server(ServerError {
            code: 1064,
            message: "You have an error in your SQL syntax".to_owned(),
            state: "42000".to_owned(),
        }));
        assert!(matches!(
            syntax,
            DbError::QueryFailed {
                code: Some(ref c),
                ..
            } if c == "1064"
        ));

        assert_eq!(
            map_mysql_err(MysqlError::Driver(DriverError::ConnectionClosed)),
            DbError::Disconnected
        );
    }

    #[test]
    fn maps_mysql_session_disconnect_errors() {
        for code in [1927, 2006, 2013, 4031] {
            let err = map_mysql_session_err(MysqlError::Server(ServerError {
                code,
                message: "MySQL server has gone away".to_owned(),
                state: "HY000".to_owned(),
            }));
            assert_eq!(err, DbError::Disconnected, "code {code} should map to Disconnected");
        }

        for kind in [
            io::ErrorKind::BrokenPipe,
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::ConnectionAborted,
            io::ErrorKind::UnexpectedEof,
            io::ErrorKind::NotConnected,
        ] {
            let err = map_mysql_session_err(MysqlError::Io(IoError::Io(io::Error::new(kind, "socket dropped"))));
            assert_eq!(err, DbError::Disconnected, "kind {kind:?} should map to Disconnected");
        }

        let refused = map_mysql_session_err(MysqlError::Io(IoError::Io(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            "connection refused",
        ))));
        assert!(matches!(refused, DbError::ConnectionFailed(_)));

        let syntax = map_mysql_session_err(MysqlError::Server(ServerError {
            code: 1064,
            message: "You have an error in your SQL syntax".to_owned(),
            state: "42000".to_owned(),
        }));
        assert!(matches!(
            syntax,
            DbError::QueryFailed {
                code: Some(ref c),
                ..
            } if c == "1064"
        ));
    }

    #[test]
    fn synthesizes_mysql_ddl_fallback_with_auto_increment_fk_and_indexes() {
        let cols = vec![
            ColumnMeta {
                name: "student_id".to_owned(),
                data_type: "int".to_owned(),
                udt_name: "int".to_owned(),
                nullable: false,
                default_expr: Some("auto_increment".to_owned()),
                is_primary_key: true,
                is_unique: false,
                comment: None,
                foreign_key: None,
            },
            ColumnMeta {
                name: "group_id".to_owned(),
                data_type: "int".to_owned(),
                udt_name: "int".to_owned(),
                nullable: true,
                default_expr: None,
                is_primary_key: false,
                is_unique: false,
                comment: None,
                foreign_key: Some(ForeignKeyRef::single(
                    "students_group_fk",
                    "group_id",
                    "greendale",
                    "study_groups",
                    "group_id",
                )),
            },
        ];
        let indexes = vec![IndexInfo {
            name: "students_group_idx".to_owned(),
            columns: vec!["group_id".to_owned()],
            is_unique: false,
            is_primary: false,
            method: "btree".to_owned(),
            size_bytes: None,
            definition: "CREATE INDEX `students_group_idx` ON `greendale`.`students` (`group_id`)".to_owned(),
        }];

        let ddl = synthesize_mysql_table_ddl("greendale", "students", &cols, &[], &indexes);
        assert!(ddl.contains("CREATE TABLE `greendale`.`students`"));
        assert!(ddl.contains("`student_id` int NOT NULL AUTO_INCREMENT"));
        assert!(!ddl.contains("DEFAULT auto_increment"));
        assert!(ddl.contains("PRIMARY KEY (`student_id`)"));
        assert!(ddl.contains(
            "CONSTRAINT `students_group_fk` FOREIGN KEY (`group_id`) REFERENCES `greendale`.`study_groups` (`group_id`)"
        ));
        assert!(ddl.contains("CREATE INDEX `students_group_idx` ON `greendale`.`students` (`group_id`);"));
    }

    #[test]
    fn maps_ssl_config_to_mysql_ssl_opts() {
        let disabled = SslConfig {
            mode: SslMode::Disable,
            ..Default::default()
        };
        assert!(build_mysql_ssl_opts(&disabled).is_none());

        let prefer = SslConfig {
            mode: SslMode::Prefer,
            ..Default::default()
        };
        let prefer_opts = build_mysql_ssl_opts(&prefer).expect("prefer ssl opts");
        assert!(prefer_opts.accept_invalid_certs());
        assert!(prefer_opts.skip_domain_validation());

        let require = SslConfig {
            mode: SslMode::Require,
            ..Default::default()
        };
        let require_opts = build_mysql_ssl_opts(&require).expect("require ssl opts");
        assert!(require_opts.accept_invalid_certs());
        assert!(require_opts.skip_domain_validation());

        let verify_ca = SslConfig {
            mode: SslMode::VerifyCa,
            ca_cert_path: Some(PathBuf::from("/tmp/ca.pem")),
            client_cert_path: Some(PathBuf::from("/tmp/client.pem")),
            client_key_path: Some(PathBuf::from("/tmp/client.key")),
        };
        let ca_opts = build_mysql_ssl_opts(&verify_ca).expect("verify-ca ssl opts");
        assert!(!ca_opts.accept_invalid_certs());
        assert!(ca_opts.skip_domain_validation());
        assert_eq!(ca_opts.root_certs().len(), 1);
        assert!(ca_opts.disable_built_in_roots());
        assert!(ca_opts.client_identity().is_some());

        let verify_full = SslConfig {
            mode: SslMode::VerifyFull,
            ca_cert_path: Some(PathBuf::from("/tmp/ca.pem")),
            ..Default::default()
        };
        let full_opts = build_mysql_ssl_opts(&verify_full).expect("verify-full ssl opts");
        assert!(!full_opts.accept_invalid_certs());
        assert!(!full_opts.skip_domain_validation());
    }

    #[test]
    fn builds_mysql_opts_sets_tcp_keepalive_only_for_tcp_hosts() {
        let tcp_profile = ConnectionProfile::new_mysql("greendale_mysql", "db.greendale.edu", "abed");
        let tcp_opts = build_mysql_opts(&tcp_profile, Some("secret"), Some("study_room"), None);
        assert_eq!(tcp_opts.tcp_keepalive(), Some(Duration::from_secs(60)));

        let mut sock_profile = ConnectionProfile::new_mysql("local_sock", "/tmp/mysql.sock", "abed");
        sock_profile.password_mode = PasswordMode::None;
        let sock_opts = build_mysql_opts(&sock_profile, None, None, None);
        assert_eq!(sock_opts.tcp_keepalive(), None);
    }

    #[test]
    fn acquire_pool_slot_times_out_when_busy_or_resetting_even_if_freed_later() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("tokio test runtime");
        rt.block_on(async {
            let slots = Arc::new(tokio::sync::Semaphore::new(MYSQL_POOL_MAX));
            let mut held = Vec::with_capacity(MYSQL_POOL_MAX);
            for _ in 0..MYSQL_POOL_MAX {
                held.push(
                    acquire_pool_slot(&slots, Duration::from_millis(20))
                        .await
                        .expect("initial permit"),
                );
            }

            let err = acquire_pool_slot(&slots, Duration::from_millis(20))
                .await
                .expect_err("saturated pool should time out");
            assert!(
                matches!(err, DbError::ConnectionFailed(ref msg) if msg.contains("all pooled connections are busy")),
                "expected busy message, got {err:?}"
            );

            // Simulate a dropped 4th connection holding its permit until async reset completes.
            let resetting_permit = held.pop().expect("fourth permit");
            let reset_task = tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(45)).await;
                drop(resetting_permit);
            });

            let reset_busy_err = acquire_pool_slot(&slots, Duration::from_millis(15))
                .await
                .expect_err("caller should stay queued behind resetting permit");
            assert!(
                matches!(reset_busy_err, DbError::ConnectionFailed(ref msg) if msg.contains("all pooled connections are busy")),
                "expected busy message while reset in flight, got {reset_busy_err:?}"
            );

            reset_task.await.expect("reset task finishes");
            let _recovered = acquire_pool_slot(&slots, Duration::from_millis(20))
                .await
                .expect("permit should be available after reset releases it");
        });
    }

    #[test]
    fn idle_pool_evicts_and_rejects_stale_returns() {
        let mut pool = IdlePool::new(VecDeque::from(["stale_a", "stale_b"]));
        let (_, gen0) = pool.pop();
        assert_eq!(gen0, 0);

        // Returning before eviction succeeds under the active generation.
        assert_eq!(pool.try_return("still_valid", gen0, MYSQL_POOL_MAX), Ok(()));

        // Eviction drains all idle entries and advances the generation.
        let drained = pool.evict();
        assert_eq!(drained, vec!["stale_a", "still_valid"]);
        let (empty, gen1) = pool.pop();
        assert_eq!(empty, None);
        assert_ne!(gen1, gen0);

        // Stale checkout/reset connections from generation 0 are rejected instead of pooled.
        assert_eq!(
            pool.try_return("stale_inflight", gen0, MYSQL_POOL_MAX),
            Err("stale_inflight")
        );
        assert_eq!(pool.pop().0, None);

        // Fresh connections matching generation 1 can still be pooled normally.
        assert_eq!(pool.try_return("fresh", gen1, MYSQL_POOL_MAX), Ok(()));
        assert_eq!(pool.pop().0, Some("fresh"));
    }

    #[test]
    fn mysql_server_version_and_unknown_quote_mode_behave_as_expected() {
        use crate::sql::{MysqlFlavor, MysqlQuoteMode, MysqlServerVersion};

        let my84 = MysqlServerVersion::from_version_str("8.4.0").expect("8.4.0");
        assert_eq!(my84.flavor, MysqlFlavor::Mysql);
        assert_eq!(my84.version_id, 80_400);

        let my80 = MysqlServerVersion::from_version_str("8.0.36-0ubuntu0.22.04.1").expect("8.0.36");
        assert_eq!(my80.flavor, MysqlFlavor::Mysql);
        assert_eq!(my80.version_id, 80_036);

        let mdb = MysqlServerVersion::from_version_str("5.5.5-10.11.6-MariaDB-1:10.11.6+maria~ubu2204")
            .expect("MariaDB 10.11.6");
        assert_eq!(mdb.flavor, MysqlFlavor::MariaDb);
        assert_eq!(mdb.version_id, 101_106);

        let concise_mdb = MysqlServerVersion::from_version_str("MariaDB 11.4.2").expect("MariaDB 11.4.2");
        assert_eq!(concise_mdb.flavor, MysqlFlavor::MariaDb);
        assert_eq!(concise_mdb.version_id, 110_402);

        // AtomicU64 packing preserves unknown None as well as every quote flag and server version.
        let unknown = AtomicU64::new(MYSQL_QUOTE_MODE_UNKNOWN);
        assert_eq!(unpack_mysql_quote_mode(unknown.load(Ordering::Acquire)), None);
        assert_eq!(pack_mysql_quote_mode(None), MYSQL_QUOTE_MODE_UNKNOWN);

        assert_eq!(
            MysqlQuoteMode::from_sql_mode("PIPES_AS_CONCAT,ANSI_QUOTES,MSSQL"),
            MysqlQuoteMode::MSSQL
        );
        assert_eq!(
            MysqlQuoteMode::from_sql_mode("MSSQL,NO_BACKSLASH_ESCAPES"),
            MysqlQuoteMode::MSSQL_AND_NO_BACKSLASH
        );

        for mode in [
            MysqlQuoteMode::DEFAULT,
            MysqlQuoteMode::ANSI_QUOTES,
            MysqlQuoteMode::NO_BACKSLASH_ESCAPES,
            MysqlQuoteMode::ANSI_AND_NO_BACKSLASH,
            MysqlQuoteMode::MSSQL,
            MysqlQuoteMode::MSSQL_AND_NO_BACKSLASH,
        ] {
            for sv in [None, Some(my80), Some(my84), Some(mdb), Some(concise_mdb)] {
                let candidate = mode.with_server_version(sv);
                assert_eq!(unpack_mysql_quote_mode(pack_mysql_quote_mode(Some(candidate))), Some(candidate));
            }
        }
    }

    #[test]
    fn mysql_live_lease_mode_and_refresh_probe_classification_work() {
        use crate::sql::{MysqlQuoteMode, MysqlServerVersion};

        let my80 = MysqlServerVersion::from_version_str("8.0.36");
        let my84 = MysqlServerVersion::from_version_str("8.4.0");
        let mdb = MysqlServerVersion::from_version_str("MariaDB 11.4.2");

        let expected = MysqlQuoteMode::DEFAULT.with_server_version(my84);
        assert_eq!(evaluate_live_mysql_mode(expected, Some(expected)), Ok(()));
        assert_eq!(
            evaluate_live_mysql_mode(
                MysqlQuoteMode::ANSI_QUOTES.with_server_version(my84),
                Some(expected)
            ),
            Err(DbError::MysqlModeMismatch)
        );
        assert_eq!(
            evaluate_live_mysql_mode(
                MysqlQuoteMode::MSSQL.with_server_version(mdb),
                Some(MysqlQuoteMode::ANSI_QUOTES.with_server_version(mdb))
            ),
            Err(DbError::MysqlModeMismatch)
        );
        assert_eq!(
            evaluate_live_mysql_mode(
                MysqlQuoteMode::DEFAULT.with_server_version(my80),
                Some(expected)
            ),
            Err(DbError::MysqlModeMismatch)
        );
        assert_eq!(
            evaluate_live_mysql_mode(
                MysqlQuoteMode::DEFAULT.with_server_version(mdb),
                Some(expected)
            ),
            Err(DbError::MysqlModeMismatch)
        );
        assert_eq!(
            evaluate_live_mysql_mode(expected, None),
            Err(DbError::MysqlModeMismatch)
        );

        assert_eq!(
            classify_mode_refresh_probe(Ok(Ok(true)), true, false),
            CheckedConnProbeAction::Ready
        );
        assert_eq!(
            classify_mode_refresh_probe(Ok(Ok(false)), true, false),
            CheckedConnProbeAction::DiscardAndRetry
        );
        assert_eq!(
            classify_mode_refresh_probe(Ok(Err(DbError::Disconnected)), false, false),
            CheckedConnProbeAction::DiscardAndRetry
        );
        assert_eq!(
            classify_mode_refresh_probe(Ok(Err(DbError::InvalidInput("bad".to_owned()))), true, false),
            CheckedConnProbeAction::DiscardAndFail(DbError::InvalidInput("bad".to_owned()))
        );
        assert_eq!(
            classify_mode_refresh_probe(Err(()), true, false),
            CheckedConnProbeAction::DiscardAndRetryAfterTimeout
        );
        assert_eq!(
            classify_mode_refresh_probe(Err(()), true, true),
            CheckedConnProbeAction::DiscardAndDisconnect
        );
        assert_eq!(
            classify_mode_refresh_probe(Err(()), false, false),
            CheckedConnProbeAction::DiscardAndDisconnect
        );

        let my84_mode = MysqlQuoteMode::DEFAULT.with_server_version(my84);
        let my84_nbs = MysqlQuoteMode::NO_BACKSLASH_ESCAPES.with_server_version(my84);
        assert!(evaluate_mid_batch_mysql_mode(my84_mode, Some(my84_mode), "SELECT 1").is_ok());
        assert!(evaluate_mid_batch_mysql_mode(my84_nbs, Some(my84_mode), "SELECT 1").is_err());
        assert!(
            evaluate_mid_batch_mysql_mode(my84_nbs, None, "SELECT 'Troy\\'; DROP TABLE students")
                .is_err()
        );
        assert!(evaluate_mid_batch_mysql_mode(my84_nbs, None, "SELECT 'Troy'").is_ok());
    }

    #[test]
    fn should_retire_mysql_editor_conn_preserves_confirmed_cancel() {
        assert!(!should_retire_mysql_editor_conn(false, false, false, false));
        assert!(!should_retire_mysql_editor_conn(true, true, false, false));
        assert!(should_retire_mysql_editor_conn(true, false, false, false));
        assert!(should_retire_mysql_editor_conn(false, false, true, false));
        assert!(should_retire_mysql_editor_conn(true, true, true, false));
        assert!(should_retire_mysql_editor_conn(true, true, false, true));

        assert!(!should_abort_mysql_stmt_dispatch(false, false));
        assert!(should_abort_mysql_stmt_dispatch(true, false));
        assert!(should_abort_mysql_stmt_dispatch(false, true));
        assert!(should_abort_mysql_stmt_dispatch(true, true));

        assert!(should_refresh_post_batch_global_mode(true, false, false, false));
        assert!(!should_refresh_post_batch_global_mode(false, false, false, false));
        assert!(!should_refresh_post_batch_global_mode(true, true, false, false));
        assert!(!should_refresh_post_batch_global_mode(true, false, true, false));
        assert!(!should_refresh_post_batch_global_mode(true, false, false, true));

        // R19-5: Cancellation arriving alongside a timed-out/empty mid-batch probe still
        // marks disconnected and discards the lease before aborting with Cancelled.
        assert_eq!(
            decide_mid_batch_probe_after_wait(Some(MidBatchProbeFailure::TimedOutOrEmpty), true),
            MidBatchProbeDecision {
                mark_disconnected: true,
                discard_lease: true,
                abort_cancelled: true,
            }
        );
        assert_eq!(
            decide_mid_batch_probe_after_wait(Some(MidBatchProbeFailure::Disconnected), false),
            MidBatchProbeDecision {
                mark_disconnected: true,
                discard_lease: true,
                abort_cancelled: false,
            }
        );
        assert_eq!(
            decide_mid_batch_probe_after_wait(Some(MidBatchProbeFailure::QueryError), false),
            MidBatchProbeDecision {
                mark_disconnected: false,
                discard_lease: false,
                abort_cancelled: false,
            }
        );
        assert_eq!(
            decide_mid_batch_probe_after_wait(None, true),
            MidBatchProbeDecision {
                mark_disconnected: false,
                discard_lease: false,
                abort_cancelled: true,
            }
        );
        assert_eq!(
            decide_mid_batch_probe_after_wait(Some(MidBatchProbeFailure::Cancelled), true),
            MidBatchProbeDecision {
                mark_disconnected: false,
                discard_lease: false,
                abort_cancelled: true,
            }
        );
        assert!(!super::mysql_stmt_may_touch_global_sql_mode_text(
            "SET SESSION sql_mode = 'NO_BACKSLASH_ESCAPES'"
        ));
        assert!(!super::mysql_stmt_may_touch_global_sql_mode_text(
            "SET @@session.sql_mode = ''"
        ));
        assert!(super::mysql_stmt_may_touch_global_sql_mode_text(
            "SET GLOBAL sql_mode = 'ANSI_QUOTES'"
        ));
        assert!(super::mysql_stmt_may_touch_global_sql_mode_text(
            "SET PERSIST sql_mode = ''"
        ));
    }

    #[test]
    fn mysql_tx_support_classifier_and_bound_mutation_planner() {
        assert_eq!(
            classify_mysql_table_transaction_support(Some("BASE TABLE"), Some("InnoDB"), Some("YES")),
            TableTransactionSupport::Supported
        );
        assert_eq!(
            classify_mysql_table_transaction_support(Some("BASE TABLE"), Some("MyISAM"), Some("NO")),
            TableTransactionSupport::Unsupported {
                engine: "MyISAM".to_owned(),
            }
        );
        assert_eq!(
            classify_mysql_table_transaction_support(Some("BASE TABLE"), Some("MEMORY"), None),
            TableTransactionSupport::Unsupported {
                engine: "MEMORY".to_owned(),
            }
        );
        assert_eq!(
            classify_mysql_table_transaction_support(Some("BASE TABLE"), Some("Aria"), Some("NO")),
            TableTransactionSupport::Unsupported {
                engine: "Aria".to_owned(),
            }
        );
        assert_eq!(
            classify_mysql_table_transaction_support(Some("BASE TABLE"), None, None),
            TableTransactionSupport::Unknown
        );
        assert!(ensure_mysql_table_transactional(TableTransactionSupport::Supported, "greendale", "students").is_ok());
        assert!(matches!(
            ensure_mysql_table_transactional(
                TableTransactionSupport::Unsupported {
                    engine: "MyISAM".to_owned(),
                },
                "greendale",
                "students",
            ),
            Err(DbError::NonTransactionalTable {
                support: TableTransactionSupport::Unsupported { engine },
                ..
            }) if engine == "MyISAM"
        ));

        let mut pk_col = test_col("code", "varchar(32)", "varchar");
        pk_col.is_primary_key = true;
        let name_col = test_col("full_name", "varchar(128)", "varchar");
        let meta_col = test_col("meta", "json", "json");
        let dec_col = test_col("precise_id", "decimal(20,0)", "decimal");
        let cols = vec![pk_col, name_col, meta_col, dec_col];

        let orig_row = vec![
            CellValue::Text("Study\\A".to_owned()),
            CellValue::Text("Troy Barnes".to_owned()),
            CellValue::Json(r#"{"dorm":"A"}"#.to_owned()),
            CellValue::Numeric("9007199254740993".to_owned()),
        ];
        let mut staged = crate::staging::StagedChanges::new();
        staged.stage_cell_edit(
            0,
            &orig_row,
            1,
            StagedCellValue::Literal(r"C:\greendale\troy's".to_owned()),
        );
        let batch = staged
            .build_commit_batch(DbEngine::Mysql, "greendale", "students", &cols)
            .expect("build commit batch");
        let plan = build_mysql_staged_mutation_plan("greendale", "students", &batch.mutations[0])
            .expect("build mysql plan");
        match plan {
            MysqlMutationPlan::Update {
                lock_sql,
                lock_params,
                exec_sql,
                exec_params,
                ..
            } => {
                assert!(lock_sql.contains("SELECT COUNT(*) FROM `greendale`.`students` WHERE "));
                assert!(lock_sql.ends_with(" FOR UPDATE"));
                assert!(lock_sql.contains("CAST(CONVERT(`code` USING utf8mb4) AS BINARY) = CAST(CONVERT(? USING utf8mb4) AS BINARY)"));
                assert!(lock_sql.contains("CAST(CONVERT(`full_name` USING utf8mb4) AS BINARY) = CAST(CONVERT(? USING utf8mb4) AS BINARY)"));
                assert!(lock_sql.contains("`meta` = CAST(? AS JSON)"));
                assert!(lock_sql.contains("`precise_id` = CAST(? AS DECIMAL(20, 0))"));
                assert_eq!(
                    lock_params[0],
                    Value::Bytes(b"Study\\A".to_vec()),
                );
                assert!(exec_sql.starts_with("UPDATE `greendale`.`students` SET `full_name` = ? WHERE "));
                assert!(!exec_sql.contains(r"\\"));
                assert_eq!(
                    exec_params[0],
                    Value::Bytes(br"C:\greendale\troy's".to_vec()),
                );
            }
            other => panic!("expected Update plan, got {other:?}"),
        }
    }

    #[test]
    fn build_mysql_opts_sets_read_only_setup_command_when_read_only() {
        let mut ro_profile = ConnectionProfile::new_mysql("Greendale MySQL RO", "127.0.0.1", "troy");
        ro_profile.safety = SafetyPolicy::ReadOnly;
        let ro_opts = build_mysql_opts(&ro_profile, None, Some("greendale"), None);
        assert_eq!(
            ro_opts.setup(),
            ["SET SESSION TRANSACTION READ ONLY"]
        );

        let mut rw_profile = ro_profile.clone();
        rw_profile.safety = SafetyPolicy::Unrestricted;
        let rw_opts = build_mysql_opts(&rw_profile, None, Some("greendale"), None);
        assert!(rw_opts.setup().is_empty());
    }

    #[test]
    fn mysql_composite_fks_and_stable_order_handle_nulls_and_ordinals() {
        let fks = super::assemble_mysql_foreign_keys(
            "greendale",
            vec![
                (
                    Some("term".to_owned()),
                    "fk_sections".to_owned(),
                    Some(String::new()),
                    Some("course_sections".to_owned()),
                    Some("semester".to_owned()),
                    Some(2),
                ),
                (
                    Some("course_code".to_owned()),
                    "fk_sections".to_owned(),
                    Some(String::new()),
                    Some("course_sections".to_owned()),
                    Some("code".to_owned()),
                    Some(1),
                ),
                (
                    Some("orphan".to_owned()),
                    "fk_gap".to_owned(),
                    None,
                    Some("course_sections".to_owned()),
                    None,
                    Some(1),
                ),
            ],
        );
        assert_eq!(fks.len(), 2);
        let broken = fks.iter().find(|f| f.constraint_name == "fk_gap").expect("fk_gap");
        assert!(!broken.is_resolvable());
        let valid = fks
            .iter()
            .find(|f| f.constraint_name == "fk_sections")
            .expect("fk_sections");
        assert_eq!(valid.target_schema, "greendale");
        assert_eq!(valid.source_columns, vec!["course_code", "term"]);
        assert_eq!(valid.target_columns, vec!["code", "semester"]);

        let mut code_col = test_col("code", "varchar(16)", "varchar");
        code_col.nullable = false;
        let mut nullable_col = test_col("nick", "varchar(32)", "varchar");
        nullable_col.nullable = true;
        let mut term_col = test_col("term", "varchar(16)", "varchar");
        term_col.nullable = false;
        let cols = vec![code_col, nullable_col, term_col];

        let idx_col = |name: Option<&str>, is_prefix: bool| super::MysqlIndexCol {
            name: name.map(str::to_owned),
            is_prefix,
        };
        let mut idx_map = HashMap::new();
        idx_map.insert(
            "u_nullable".to_owned(),
            super::MysqlIndexMeta {
                is_unique: true,
                cols: vec![idx_col(Some("nick"), false)],
            },
        );
        idx_map.insert(
            "u_expr".to_owned(),
            super::MysqlIndexMeta {
                is_unique: true,
                cols: vec![idx_col(None, false)],
            },
        );
        idx_map.insert(
            "u_prefix".to_owned(),
            super::MysqlIndexMeta {
                is_unique: true,
                cols: vec![idx_col(Some("code"), true)],
            },
        );
        idx_map.insert(
            "u_nonnull".to_owned(),
            super::MysqlIndexMeta {
                is_unique: true,
                cols: vec![idx_col(Some("code"), false), idx_col(Some("term"), false)],
            },
        );
        let order = super::select_mysql_stable_order_keys(&cols, &idx_map);
        assert_eq!(
            order,
            vec![
                StableOrderKey::Column {
                    name: "code".to_owned(),
                    nullable: false,
                    collation: None,
                },
                StableOrderKey::Column {
                    name: "term".to_owned(),
                    nullable: false,
                    collation: None,
                },
            ]
        );

        let mut prefix_only = HashMap::new();
        prefix_only.insert(
            "u_prefix".to_owned(),
            super::MysqlIndexMeta {
                is_unique: true,
                cols: vec![idx_col(Some("code"), true)],
            },
        );
        assert!(super::select_mysql_stable_order_keys(&cols, &prefix_only).is_empty());

        let req = PageRequest {
            schema: "greendale".to_owned(),
            table: "sections".to_owned(),
            filter_where: None,
            sort: Some(crate::engine::SortSpec {
                column: "code".to_owned(),
                ascending: true,
            }),
            limit: 50,
            offset: 0,
        };
        let sql = build_mysql_page_sql("greendale", "sections", &cols, &req, &order, 51, 0);
        assert!(sql.ends_with("ORDER BY (`code` IS NULL) ASC, `code` ASC, `term` ASC LIMIT 51 OFFSET 0"));
    }
}
