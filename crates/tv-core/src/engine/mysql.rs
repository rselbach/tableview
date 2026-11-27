use std::collections::{BTreeMap, HashMap};
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use mysql_async::consts::{ColumnFlags, ColumnType};
use mysql_async::prelude::Queryable;
use mysql_async::{
    ClientIdentity, Column, DriverError, Error as MysqlError, IoError, Opts, OptsBuilder, Pool, PoolConstraints,
    PoolOpts, Row, SslOpts, Value,
};
use uuid::Uuid;

use crate::connection::{ConnectionProfile, DbEngine, PasswordMode, SslConfig, SslMode};
use crate::engine::{
    CellValue, ColumnMeta, ColumnSummary, DatabaseInfo, DbError, DbObject, DbObjectKind, DbSession, ExecutedSql,
    FkPeekRow, ForeignKeyRef, IndexInfo, PageRequest, QueryBatchResult, QueryStatementResult, SchemaInfo, ServerInfo,
    TablePage, TableStats, TableStructure, TransactionOutcome,
};
use crate::explain::{ExplainPlan, build_explain_sql_for_engine};
use crate::staging::build_fk_where_clause_for_engine;

static CRYPTO_PROVIDER_INIT: OnceLock<()> = OnceLock::new();
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

fn ensure_crypto_provider() {
    CRYPTO_PROVIDER_INIT.get_or_init(|| {
        drop(rustls::crypto::ring::default_provider().install_default());
    });
}

/// Active MySQL / MariaDB database session backed by `mysql_async`.
pub struct MysqlSession {
    pool: Pool,
    server_info: ServerInfo,
    active_conn_id: Arc<AtomicU64>,
    tx_lock: tokio::sync::Mutex<()>,
}

struct ActiveConnGuard<'a>(&'a AtomicU64);

impl Drop for ActiveConnGuard<'_> {
    fn drop(&mut self) {
        self.0.store(0, Ordering::Release);
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

        let (pool, mut conn) = connect_pool_with_fallback(profile, password, target_db).await?;

        let info_sql = "SELECT VERSION(), COALESCE(DATABASE(), ''), CURRENT_USER()";
        let start = Instant::now();
        let row: Option<Row> = conn.query_first(info_sql).await.map_err(map_mysql_err)?;
        let duration_ms = start.elapsed().as_millis() as u64;
        drop(conn);

        let Some(row) = row else {
            drop(pool.disconnect().await);
            return Err(DbError::ConnectionFailed(
                "MySQL server returned no rows for version query".to_owned(),
            ));
        };

        let raw_version = value_to_string(row.as_ref(0).unwrap_or(&Value::NULL));
        let current_database = value_to_string(row.as_ref(1).unwrap_or(&Value::NULL));
        let current_user = value_to_string(row.as_ref(2).unwrap_or(&Value::NULL));

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
                pool,
                server_info,
                active_conn_id: Arc::new(AtomicU64::new(0)),
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
        drop(session.pool.disconnect().await);
        Ok(info)
    }

    async fn get_conn(&self) -> Result<mysql_async::Conn, DbError> {
        match tokio::time::timeout(CONNECT_TIMEOUT, self.pool.get_conn()).await {
            Ok(Ok(conn)) => Ok(conn),
            Ok(Err(err)) => Err(map_mysql_err(err)),
            Err(_) => Err(DbError::ConnectionFailed("connection timed out".to_owned())),
        }
    }

    async fn fetch_column_metas(&self, schema: &str, table: &str) -> Result<(Vec<ColumnMeta>, ExecutedSql), DbError> {
        let mut conn = self.get_conn().await?;
        let col_sql = "SELECT COLUMN_NAME, COLUMN_TYPE, DATA_TYPE, IS_NULLABLE, COLUMN_DEFAULT, \
                       COLUMN_KEY, COLUMN_COMMENT, EXTRA \
                       FROM information_schema.COLUMNS \
                       WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? \
                       ORDER BY ORDINAL_POSITION ASC";
        let start = Instant::now();
        let col_rows: Vec<Row> = conn.exec(col_sql, (schema, table)).await.map_err(map_mysql_err)?;

        let fk_sql = "SELECT COLUMN_NAME, CONSTRAINT_NAME, REFERENCED_TABLE_SCHEMA, \
                      REFERENCED_TABLE_NAME, REFERENCED_COLUMN_NAME \
                      FROM information_schema.KEY_COLUMN_USAGE \
                      WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND REFERENCED_TABLE_NAME IS NOT NULL \
                      ORDER BY CONSTRAINT_NAME ASC, ORDINAL_POSITION ASC";
        let fk_rows: Vec<Row> = conn.exec(fk_sql, (schema, table)).await.map_err(map_mysql_err)?;

        let stats_sql = "SELECT INDEX_NAME, NON_UNIQUE, COLUMN_NAME, SEQ_IN_INDEX \
                         FROM information_schema.STATISTICS \
                         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? \
                         ORDER BY INDEX_NAME ASC, SEQ_IN_INDEX ASC";
        let stat_rows: Vec<Row> = conn.exec(stats_sql, (schema, table)).await.map_err(map_mysql_err)?;
        drop(conn);

        let mut fks: HashMap<String, ForeignKeyRef> = HashMap::new();
        for row in fk_rows {
            let col_name = value_to_string(row.as_ref(0).unwrap_or(&Value::NULL));
            fks.entry(col_name).or_insert_with(|| ForeignKeyRef {
                constraint_name: value_to_string(row.as_ref(1).unwrap_or(&Value::NULL)),
                target_schema: value_to_string(row.as_ref(2).unwrap_or(&Value::NULL)),
                target_table: value_to_string(row.as_ref(3).unwrap_or(&Value::NULL)),
                target_column: value_to_string(row.as_ref(4).unwrap_or(&Value::NULL)),
            });
        }

        let mut index_cols: HashMap<String, (bool, Vec<String>)> = HashMap::new();
        for row in stat_rows {
            let index_name = value_to_string(row.as_ref(0).unwrap_or(&Value::NULL));
            let non_unique = value_to_opt_i64(row.as_ref(1).unwrap_or(&Value::NULL)).unwrap_or(1);
            let col_name = value_to_string(row.as_ref(2).unwrap_or(&Value::NULL));
            let entry = index_cols
                .entry(index_name)
                .or_insert_with(|| (non_unique == 0, Vec::new()));
            entry.1.push(col_name);
        }

        let mut pk_cols = std::collections::HashSet::new();
        let mut unique_cols = std::collections::HashSet::new();
        for (idx_name, (is_unique, cols)) in index_cols {
            if idx_name == "PRIMARY" {
                for c in cols {
                    pk_cols.insert(c);
                }
            } else if is_unique && cols.len() == 1
                && let Some(only_col) = cols.into_iter().next()
            {
                unique_cols.insert(only_col);
            }
        }

        let columns = col_rows
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

        let log = ExecutedSql {
            sql: format!(
                "{col_sql} -- ? = '{}', ? = '{}'",
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
impl DbSession for MysqlSession {
    fn engine(&self) -> DbEngine {
        DbEngine::Mysql
    }

    fn server_info(&self) -> &ServerInfo {
        &self.server_info
    }

    async fn list_databases(&self) -> Result<(Vec<DatabaseInfo>, ExecutedSql), DbError> {
        let mut conn = self.get_conn().await?;
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
                let rows: Vec<Row> = conn.query(fallback_sql).await.map_err(map_mysql_err)?;
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
            Err(err) => Err(map_mysql_err(err)),
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

        let mut conn = self.get_conn().await?;
        let sql = "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA \
                   ORDER BY (SCHEMA_NAME IN ('information_schema', 'mysql', 'performance_schema', 'sys')) ASC, \
                            SCHEMA_NAME ASC";
        let start = Instant::now();
        let rows: Vec<Row> = conn.query(sql).await.map_err(map_mysql_err)?;
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
        let mut conn = self.get_conn().await?;
        let sql = "SELECT TABLE_NAME, TABLE_TYPE, TABLE_ROWS, (DATA_LENGTH + INDEX_LENGTH) \
                   FROM information_schema.TABLES \
                   WHERE TABLE_SCHEMA = ? \
                   ORDER BY TABLE_NAME ASC";
        let start = Instant::now();
        let rows: Vec<Row> = conn.exec(sql, (schema,)).await.map_err(map_mysql_err)?;
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
        let mut conn = self.get_conn().await?;
        let sql = "SELECT COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_KEY \
                   FROM information_schema.COLUMNS \
                   WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? \
                   ORDER BY ORDINAL_POSITION ASC";
        let start = Instant::now();
        let rows: Vec<Row> = conn.exec(sql, (schema, object)).await.map_err(map_mysql_err)?;
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

        let sql = build_mysql_page_sql(&req.schema, &req.table, &columns, req, limit + 1, offset);
        let mut conn = self.get_conn().await?;
        let start = Instant::now();
        let mysql_rows: Vec<Row> = conn.query(&sql).await.map_err(map_mysql_err)?;
        let elapsed_ms = start.elapsed().as_millis() as u64;

        let mut logs = vec![ExecutedSql {
            sql,
            duration_ms: elapsed_ms,
            error: None,
        }];

        let has_more = mysql_rows.len() > limit as usize;
        let row_count = mysql_rows.len().min(limit as usize);
        let mut decoded_rows = Vec::with_capacity(row_count);
        for row in mysql_rows.into_iter().take(limit as usize) {
            let wire_cols = row.columns_ref();
            let mut cells = Vec::with_capacity(columns.len());
            for (idx, col) in columns.iter().enumerate() {
                let val = row.as_ref(idx).unwrap_or(&Value::NULL);
                let wire_col = wire_cols.get(idx);
                cells.push(decode_mysql_cell(val, wire_col, Some(col)));
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
            let est_sql = "SELECT TABLE_ROWS FROM information_schema.TABLES \
                           WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?";
            let est_start = Instant::now();
            let est_row: Option<Row> = conn
                .exec_first(est_sql, (&req.schema, &req.table))
                .await
                .ok()
                .flatten();
            let est = est_row
                .as_ref()
                .and_then(|r| r.as_ref(0))
                .and_then(value_to_opt_i64);
            logs.push(ExecutedSql {
                sql: format!(
                    "{est_sql} -- ? = '{}', ? = '{}'",
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
        let mut logs = Vec::new();
        let (columns, col_log) = self.fetch_column_metas(schema, table).await?;
        logs.push(col_log);

        let mut conn = self.get_conn().await?;

        let idx_sql = "SELECT INDEX_NAME, NON_UNIQUE, COLUMN_NAME, INDEX_TYPE, SEQ_IN_INDEX \
                       FROM information_schema.STATISTICS \
                       WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? \
                       ORDER BY (INDEX_NAME = 'PRIMARY') DESC, NON_UNIQUE ASC, INDEX_NAME ASC, SEQ_IN_INDEX ASC";
        let idx_start = Instant::now();
        let idx_rows: Vec<Row> = conn.exec(idx_sql, (schema, table)).await.map_err(map_mysql_err)?;
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

        let stats_sql = "SELECT TABLE_TYPE, DATA_LENGTH, INDEX_LENGTH, TABLE_ROWS, TABLE_COMMENT \
                         FROM information_schema.TABLES \
                         WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?";
        let stats_start = Instant::now();
        let stats_row: Option<Row> = conn.exec_first(stats_sql, (schema, table)).await.map_err(map_mysql_err)?;
        logs.push(ExecutedSql {
            sql: format!(
                "{stats_sql} -- ? = '{}', ? = '{}'",
                schema.replace('\'', "''"),
                table.replace('\'', "''")
            ),
            duration_ms: stats_start.elapsed().as_millis() as u64,
            error: None,
        });

        let (is_view, data_bytes, index_bytes, estimated_rows, comment) = if let Some(row) = stats_row {
            let table_type = value_to_string(row.as_ref(0).unwrap_or(&Value::NULL)).to_ascii_uppercase();
            let is_view = table_type.contains("VIEW");
            let data_bytes = value_to_opt_i64(row.as_ref(1).unwrap_or(&Value::NULL));
            let index_bytes = value_to_opt_i64(row.as_ref(2).unwrap_or(&Value::NULL));
            let estimated_rows = value_to_opt_i64(row.as_ref(3).unwrap_or(&Value::NULL));
            let comment = value_to_opt_string(row.as_ref(4).unwrap_or(&Value::NULL))
                .filter(|s| !s.trim().is_empty())
                .filter(|s| !(is_view && s.eq_ignore_ascii_case("VIEW")));
            (is_view, data_bytes, index_bytes, estimated_rows, comment)
        } else {
            (false, None, None, None, None)
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
                    None => synthesize_mysql_table_ddl(schema, table, &columns, &indexes),
                }
            }
            Ok(None) => {
                logs.push(ExecutedSql {
                    sql: show_sql,
                    duration_ms: show_start.elapsed().as_millis() as u64,
                    error: None,
                });
                synthesize_mysql_table_ddl(schema, table, &columns, &indexes)
            }
            Err(err) => {
                logs.push(ExecutedSql {
                    sql: show_sql,
                    duration_ms: show_start.elapsed().as_millis() as u64,
                    error: Some(err.to_string()),
                });
                synthesize_mysql_table_ddl(schema, table, &columns, &indexes)
            }
        };

        Ok((
            TableStructure {
                columns,
                indexes,
                stats: TableStats {
                    total_bytes,
                    data_bytes,
                    index_bytes,
                    estimated_rows,
                    comment,
                    ddl,
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
        let mut conn = self.get_conn().await?;
        let conn_id = u64::from(conn.id());
        self.active_conn_id.store(conn_id, Ordering::Release);
        let _guard = ActiveConnGuard(&self.active_conn_id);

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
                    } else {
                        drop(result);
                    }

                    let elapsed_ms = start.elapsed().as_millis() as u64;
                    if let Some(err) = stream_err {
                        let mapped = map_mysql_err(err);
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
                }
                Err(mysql_err) => {
                    let elapsed_ms = start.elapsed().as_millis() as u64;
                    let mapped = map_mysql_err(mysql_err);
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

    async fn execute_transaction(
        &self,
        statements: &[String],
    ) -> Result<(TransactionOutcome, Vec<ExecutedSql>), DbError> {
        if statements.is_empty() {
            return Err(DbError::InvalidInput(
                "no SQL statements to execute in transaction".to_owned(),
            ));
        }

        let _tx_guard = self.tx_lock.lock().await;
        let mut conn = self.get_conn().await?;
        let total_start = Instant::now();
        let mut logs = Vec::with_capacity(statements.len() + 2);
        let mut total_rows_affected = 0u64;
        let mut executed = 0usize;

        let begin_start = Instant::now();
        if let Err(err) = conn.query_drop("START TRANSACTION").await {
            let mapped = map_mysql_err(err);
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
            let stmt = raw_stmt.trim();
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
                    let mapped = map_mysql_err(err);
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
                let mapped = map_mysql_err(err);
                drop(conn.query_drop("ROLLBACK").await);
                Err(mapped)
            }
        }
    }

    async fn explain_sql(&self, statement: &str, analyze: bool) -> Result<(ExplainPlan, ExecutedSql), DbError> {
        let trimmed = statement.trim();
        if trimmed.is_empty() {
            return Err(DbError::InvalidInput(
                "cannot explain an empty SQL statement".to_owned(),
            ));
        }

        let explain_query = build_explain_sql_for_engine(DbEngine::Mysql, trimmed, analyze);
        let mut conn = self.get_conn().await?;
        let start = Instant::now();
        let rows: Vec<Row> = conn.query(&explain_query).await.map_err(map_mysql_err)?;
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

    async fn fetch_fk_row(
        &self,
        fk: &ForeignKeyRef,
        value: &CellValue,
        source_col: &ColumnMeta,
    ) -> Result<(FkPeekRow, ExecutedSql), DbError> {
        let Some(where_clause) = build_fk_where_clause_for_engine(DbEngine::Mysql, &fk.target_column, value, source_col)
        else {
            return Err(DbError::InvalidInput("cannot peek NULL foreign-key value".to_owned()));
        };

        let target_schema = if fk.target_schema.trim().is_empty() {
            self.server_info.current_database.as_str()
        } else {
            fk.target_schema.as_str()
        };

        let (target_columns, _) = self
            .fetch_column_metas(target_schema, &fk.target_table)
            .await?;
        if target_columns.is_empty() {
            return Err(DbError::InvalidInput(format!(
                "target table {}.{} has no columns",
                target_schema, fk.target_table
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
        let sql = build_mysql_page_sql(target_schema, &fk.target_table, &target_columns, &req, 1, 0);

        let mut conn = self.get_conn().await?;
        let start = Instant::now();
        let first_row: Option<Row> = conn.query_first(&sql).await.map_err(map_mysql_err)?;
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

    async fn cancel_running_query(&self) -> Result<(), DbError> {
        let conn_id = self.active_conn_id.load(Ordering::Acquire);
        if conn_id == 0 {
            return Ok(());
        }
        let mut conn = self.get_conn().await?;
        let kill_sql = format!("KILL QUERY {conn_id}");
        match conn.query_drop(kill_sql).await {
            Ok(()) => Ok(()),
            // ER_NO_SUCH_THREAD: query completed before KILL QUERY arrived
            Err(MysqlError::Server(se)) if se.code == 1094 => Ok(()),
            Err(err) => Err(map_mysql_err(err)),
        }
    }

    async fn ping(&self) -> Result<(), DbError> {
        let mut conn = self.get_conn().await?;
        conn.ping().await.map_err(map_mysql_err)
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

async fn connect_pool_with_fallback(
    profile: &ConnectionProfile,
    password: Option<&str>,
    target_db: Option<&str>,
) -> Result<(Pool, mysql_async::Conn), DbError> {
    let host = profile.host.trim();
    let initial_ssl = if host.starts_with('/') {
        None
    } else {
        build_mysql_ssl_opts(&profile.ssl)
    };

    let opts = build_mysql_opts(profile, password, target_db, initial_ssl);
    let pool = Pool::new(opts);

    match tokio::time::timeout(CONNECT_TIMEOUT, pool.get_conn()).await {
        Ok(Ok(conn)) => Ok((pool, conn)),
        Ok(Err(err))
            if profile.ssl.mode == SslMode::Prefer && !host.starts_with('/') && is_prefer_tls_fallback_error(&err) =>
        {
            drop(pool.disconnect().await);
            let plain_opts = build_mysql_opts(profile, password, target_db, None);
            let plain_pool = Pool::new(plain_opts);
            match tokio::time::timeout(CONNECT_TIMEOUT, plain_pool.get_conn()).await {
                Ok(Ok(conn)) => Ok((plain_pool, conn)),
                Ok(Err(plain_err)) => {
                    drop(plain_pool.disconnect().await);
                    Err(map_mysql_err(plain_err))
                }
                Err(_) => {
                    drop(plain_pool.disconnect().await);
                    Err(DbError::ConnectionFailed("connection timed out".to_owned()))
                }
            }
        }
        Ok(Err(err)) => {
            drop(pool.disconnect().await);
            Err(map_mysql_err(err))
        }
        Err(_) => {
            drop(pool.disconnect().await);
            Err(DbError::ConnectionFailed("connection timed out".to_owned()))
        }
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
            .prefer_socket(false);
    }

    builder = builder.user(Some(profile.user.trim()));
    if profile.password_mode != PasswordMode::None {
        builder = builder.pass(password);
    }
    builder = builder.db_name(target_db);
    if let Some(constraints) = PoolConstraints::new(1, 4) {
        builder = builder.pool_opts(PoolOpts::default().with_constraints(constraints));
    }
    if let Some(ssl) = ssl_opts {
        builder = builder.ssl_opts(ssl);
    }
    Opts::from(builder)
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

/// Builds a paginated MySQL `SELECT` statement with backtick quoting and `NULLS LAST` emulation.
pub fn build_mysql_page_sql(
    schema: &str,
    table: &str,
    columns: &[ColumnMeta],
    req: &PageRequest,
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

    if let Some(sort) = &req.sort {
        let quoted_col = DbEngine::Mysql.quote_ident(&sort.column);
        let dir = if sort.ascending { "ASC" } else { "DESC" };
        sql.push_str(&format!(" ORDER BY ({quoted_col} IS NULL) ASC, {quoted_col} {dir}"));
    }

    sql.push_str(&format!(" LIMIT {} OFFSET {}", fetch_limit.max(1), offset.max(0)));
    sql
}

/// Synthesizes a readable MySQL `CREATE TABLE` DDL statement when `SHOW CREATE TABLE` is unavailable.
pub fn synthesize_mysql_table_ddl(
    schema: &str,
    table: &str,
    columns: &[ColumnMeta],
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

    for col in columns {
        if let Some(fk) = &col.foreign_key {
            let target_ref = DbEngine::Mysql.qualify_ident(&fk.target_schema, &fk.target_table);
            clauses.push(format!(
                "    CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {target_ref} ({})",
                DbEngine::Mysql.quote_ident(&fk.constraint_name),
                DbEngine::Mysql.quote_ident(&col.name),
                DbEngine::Mysql.quote_ident(&fk.target_column),
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
        let sql = build_mysql_page_sql("greendale", "students", &cols, &req, 51, 100);
        assert_eq!(
            sql,
            "SELECT `student_id`, `full``name` FROM `greendale`.`students` \
             WHERE (enrolled = 1) ORDER BY (`full``name` IS NULL) ASC, `full``name` DESC LIMIT 51 OFFSET 100"
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
                foreign_key: Some(ForeignKeyRef {
                    constraint_name: "students_group_fk".to_owned(),
                    target_schema: "greendale".to_owned(),
                    target_table: "study_groups".to_owned(),
                    target_column: "group_id".to_owned(),
                }),
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

        let ddl = synthesize_mysql_table_ddl("greendale", "students", &cols, &indexes);
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
}
