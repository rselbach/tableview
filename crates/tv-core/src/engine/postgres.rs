use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
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

fn ensure_crypto_provider() {
    CRYPTO_PROVIDER_INIT.get_or_init(|| {
        drop(rustls::crypto::ring::default_provider().install_default());
    });
}

/// Active PostgreSQL database session backed by `tokio-postgres`.
pub struct PostgresSession {
    client: Arc<tokio_postgres::Client>,
    server_info: ServerInfo,
    ssl: SslConfig,
    use_tls_for_cancel: bool,
    disconnected: Arc<AtomicBool>,
    conn_task: Mutex<Option<JoinHandle<()>>>,
    tx_lock: tokio::sync::Mutex<()>,
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
        let (client, conn_task, disconnected, used_tls) =
            match connect_raw(profile, password, effective_db).await {
                Ok(ok) => ok,
                Err(err) if target_db.is_none() && is_missing_database_error(&err) && profile.user.trim() != "postgres" => {
                    connect_raw(profile, password, profile.user.trim()).await?
                }
                Err(err) => return Err(err),
            };

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
            conn_task: Mutex::new(Some(conn_task)),
            tx_lock: tokio::sync::Mutex::new(()),
        };

        let (info, log) = session.fetch_server_info().await?;
        session.server_info = info;
        Ok((session, vec![log]))
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
        if self.disconnected.load(Ordering::Acquire) || self.client.is_closed() {
            return Err(DbError::Disconnected);
        }
        Ok(())
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
            .map_err(map_pg_err)?;

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
            .map_err(map_pg_err)?;

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

    async fn list_databases(&self) -> Result<(Vec<DatabaseInfo>, ExecutedSql), DbError> {
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
                let fallback_sql = "SELECT d.datname, COALESCE(pg_catalog.pg_get_userbyid(d.datdba), ''), NULL::bigint \
                                    FROM pg_catalog.pg_database d \
                                    WHERE d.datistemplate = false \
                                    ORDER BY d.datname ASC";
                let rows = self.client.query(fallback_sql, &[]).await.map_err(map_pg_err)?;
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
            Err(err) => return Err(map_pg_err(err)),
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
        self.ensure_alive()?;
        let sql = "SELECT nspname FROM pg_catalog.pg_namespace \
                   WHERE nspname NOT LIKE 'pg_temp_%' AND nspname NOT LIKE 'pg_toast_temp_%' \
                   ORDER BY (nspname = 'public') DESC, \
                            (nspname LIKE 'pg_%' OR nspname = 'information_schema') ASC, \
                            nspname ASC";
        let start = Instant::now();
        let rows = self.client.query(sql, &[]).await.map_err(map_pg_err)?;
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
        self.ensure_alive()?;
        let sql = "SELECT c.relname, c.relkind::text, \
                   CASE WHEN c.reltuples >= 0 THEN c.reltuples::bigint ELSE NULL END, \
                   CASE WHEN c.relkind IN ('r', 'p', 'm') THEN pg_catalog.pg_total_relation_size(c.oid) ELSE NULL END \
                   FROM pg_catalog.pg_class c \
                   JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                   WHERE n.nspname = $1 AND c.relkind IN ('r', 'p', 'v', 'm', 'f') \
                   ORDER BY c.relname ASC";
        let start = Instant::now();
        let rows = self.client.query(sql, &[&schema]).await.map_err(map_pg_err)?;
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
        let rows = self.client.query(sql, &[&schema, &object]).await.map_err(map_pg_err)?;
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
        self.ensure_alive()?;
        self.client.simple_query("SELECT 1").await.map_err(map_pg_err)?;
        Ok(())
    }

    async fn fetch_table_page(&self, req: &PageRequest) -> Result<(TablePage, Vec<ExecutedSql>), DbError> {
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
        let start = Instant::now();
        let pg_rows = self.client.query(&sql, &[]).await.map_err(map_pg_err)?;
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
                let raw = row.try_get::<usize, Option<String>>(idx).map_err(map_pg_err)?;
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
                _ => None,
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
            .map_err(map_pg_err)?;
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
            .map_err(map_pg_err)?;
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
        self.ensure_alive()?;
        let batch_start = Instant::now();
        let mut results = Vec::new();
        let mut logs = Vec::new();
        let mut cancelled = false;

        for raw_stmt in statements {
            let stmt = raw_stmt.trim();
            if stmt.is_empty() {
                continue;
            }
            let start = Instant::now();
            let prepared_cols: Option<Vec<(String, String)>> = self
                .client
                .prepare(stmt)
                .await
                .ok()
                .map(|s| {
                    s.columns()
                        .iter()
                        .map(|c| (c.name().to_owned(), c.type_().name().to_owned()))
                        .collect()
                });

            match self.client.simple_query(stmt).await {
                Ok(messages) => {
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
                    let elapsed_ms = start.elapsed().as_millis() as u64;
                    let mapped = map_pg_err(pg_err);
                    if matches!(mapped, DbError::Cancelled) {
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

    async fn cancel_running_query(&self) -> Result<(), DbError> {
        self.ensure_alive()?;
        let token = self.client.cancel_token();
        if !self.use_tls_for_cancel {
            token.cancel_query(NoTls).await.map_err(map_pg_err)?;
            return Ok(());
        }
        let tls_cfg = build_rustls_config(&self.ssl)?;
        let connector = MakeRustlsConnect::new(tls_cfg);
        match token.cancel_query(connector).await {
            Ok(()) => Ok(()),
            Err(err) if self.ssl.mode == SslMode::Prefer && err.as_db_error().is_none() => {
                token.cancel_query(NoTls).await.map_err(map_pg_err)
            }
            Err(err) => Err(map_pg_err(err)),
        }
    }

    async fn execute_transaction(
        &self,
        statements: &[String],
    ) -> Result<(TransactionOutcome, Vec<ExecutedSql>), DbError> {
        self.ensure_alive()?;
        if statements.is_empty() {
            return Err(DbError::InvalidInput("no SQL statements to execute in transaction".to_owned()));
        }
        let _tx_guard = self.tx_lock.lock().await;
        let total_start = Instant::now();
        let mut logs = Vec::with_capacity(statements.len() + 2);
        let mut total_rows_affected = 0u64;
        let mut executed = 0usize;

        let begin_start = Instant::now();
        if let Err(pg_err) = self.client.simple_query("BEGIN").await {
            let mapped = map_pg_err(pg_err);
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
            let stmt = raw_stmt.trim();
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
                    let mapped = map_pg_err(pg_err);
                    drop(self.client.simple_query("ROLLBACK").await);
                    return Err(mapped);
                }
            }
        }

        let commit_start = Instant::now();
        match self.client.simple_query("COMMIT").await {
            Ok(_) => {
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
                let mapped = map_pg_err(pg_err);
                drop(self.client.simple_query("ROLLBACK").await);
                Err(mapped)
            }
        }
    }

    async fn explain_sql(&self, statement: &str, analyze: bool) -> Result<(ExplainPlan, ExecutedSql), DbError> {
        self.ensure_alive()?;
        let trimmed = statement.trim();
        if trimmed.is_empty() {
            return Err(DbError::InvalidInput("cannot explain an empty SQL statement".to_owned()));
        }
        let explain_query = build_explain_sql(trimmed, analyze);
        let start = Instant::now();
        let messages = match self.client.simple_query(&explain_query).await {
            Ok(msgs) => msgs,
            Err(err) => return Err(map_pg_err(err)),
        };
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

    async fn fetch_fk_row(
        &self,
        fk: &ForeignKeyRef,
        value: &CellValue,
        source_col: &ColumnMeta,
    ) -> Result<(FkPeekRow, ExecutedSql), DbError> {
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
        let pg_rows = self.client.query(&sql, &[]).await.map_err(map_pg_err)?;
        let elapsed_ms = start.elapsed().as_millis() as u64;

        let row = if let Some(first) = pg_rows.into_iter().next() {
            let mut cells = Vec::with_capacity(target_columns.len());
            for (idx, col) in target_columns.iter().enumerate() {
                let raw = first.try_get::<usize, Option<String>>(idx).map_err(map_pg_err)?;
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

async fn connect_raw(
    profile: &ConnectionProfile,
    password: Option<&str>,
    database: &str,
) -> Result<(tokio_postgres::Client, JoinHandle<()>, Arc<AtomicBool>, bool), DbError> {
    let mut cfg = Config::new();
    let host = profile.host.trim();
    cfg.host(host);
    if !host.starts_with('/') {
        cfg.port(profile.port);
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

    let use_tls = match profile.ssl.mode {
        SslMode::Disable => false,
        SslMode::Prefer => !host.starts_with('/'),
        SslMode::Require | SslMode::VerifyCa | SslMode::VerifyFull => true,
    };

    if !use_tls {
        let (c, h, d) = connect_no_tls(&cfg).await?;
        return Ok((c, h, d, false));
    }

    let tls_cfg = build_rustls_config(&profile.ssl)?;
    let connector = MakeRustlsConnect::new(tls_cfg);
    match tokio::time::timeout(CONNECT_TIMEOUT, cfg.connect(connector)).await {
        Ok(Ok((client, connection))) => {
            let (handle, disconnected) = spawn_connection_driver(connection);
            Ok((client, handle, disconnected, true))
        }
        Ok(Err(err)) => {
            if profile.ssl.mode == SslMode::Prefer && err.as_db_error().is_none() {
                let (c, h, d) = connect_no_tls(&cfg).await?;
                Ok((c, h, d, false))
            } else {
                Err(map_pg_err(err))
            }
        }
        Err(_) => Err(DbError::ConnectionFailed("connection timed out".to_owned())),
    }
}

async fn connect_no_tls(
    cfg: &Config,
) -> Result<(tokio_postgres::Client, JoinHandle<()>, Arc<AtomicBool>), DbError> {
    match tokio::time::timeout(CONNECT_TIMEOUT, cfg.connect(NoTls)).await {
        Ok(Ok((client, connection))) => {
            let (handle, disconnected) = spawn_connection_driver(connection);
            Ok((client, handle, disconnected))
        }
        Ok(Err(err)) => Err(map_pg_err(err)),
        Err(_) => Err(DbError::ConnectionFailed("connection timed out".to_owned())),
    }
}

fn spawn_connection_driver<S, T>(connection: tokio_postgres::Connection<S, T>) -> (JoinHandle<()>, Arc<AtomicBool>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let disconnected = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&disconnected);
    let handle = tokio::spawn(async move {
        drop(connection.await);
        flag.store(true, Ordering::Release);
    });
    (handle, disconnected)
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
    let file = File::open(path).map_err(|e| DbError::TlsConfig(format!("open {}: {e}", path.display())))?;
    let mut reader = BufReader::new(file);
    rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| DbError::TlsConfig(format!("parse PEM certs {}: {e}", path.display())))
}

fn load_private_key(path: &Path) -> Result<PrivateKeyDer<'static>, DbError> {
    let file = File::open(path).map_err(|e| DbError::TlsConfig(format!("open {}: {e}", path.display())))?;
    let mut reader = BufReader::new(file);
    rustls_pemfile::private_key(&mut reader)
        .map_err(|e| DbError::TlsConfig(format!("parse private key {}: {e}", path.display())))?
        .ok_or_else(|| DbError::TlsConfig(format!("no private key found in {}", path.display())))
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
    use super::concise_pg_version;

    #[test]
    fn parses_concise_postgresql_version() {
        assert_eq!(
            concise_pg_version("PostgreSQL 18.4 (Postgres.app) on aarch64-apple-darwin23.6.0, compiled by Apple clang"),
            "PostgreSQL 18.4"
        );
        assert_eq!(concise_pg_version("PostgreSQL 14.8 on x86_64-pc-linux-gnu"), "PostgreSQL 14.8");
    }
}
