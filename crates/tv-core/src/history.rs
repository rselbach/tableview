use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, TimeZone, Utc};
use directories::ProjectDirs;
use rusqlite::{Connection, OptionalExtension, params};
use thiserror::Error;

use crate::ids::{ConnectionId, SavedQueryId};

const MAX_HISTORY_ROWS: i64 = 2000;

/// Recorded SQL execution entry in the workspace History drawer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryHistoryEntry {
    pub id: i64,
    pub connection_id: Option<ConnectionId>,
    pub connection_name: String,
    pub database_name: Option<String>,
    pub sql: String,
    pub executed_at: DateTime<Utc>,
    pub duration_ms: u64,
    pub rows_count: Option<u64>,
    pub error: Option<String>,
}

/// Parameters required to insert one query history entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewHistoryEntry {
    pub connection_id: Option<ConnectionId>,
    pub connection_name: String,
    pub database_name: Option<String>,
    pub sql: String,
    pub duration_ms: u64,
    pub rows_count: Option<u64>,
    pub error: Option<String>,
}

/// Named reusable SQL script saved by the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedQuery {
    pub id: SavedQueryId,
    pub connection_id: Option<ConnectionId>,
    pub folder: Option<String>,
    pub name: String,
    pub sql: String,
    pub updated_at: DateTime<Utc>,
}

/// Errors produced when reading or writing local query history and saved queries.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum QueryStoreError {
    #[error("unable to locate platform data directory")]
    NoDataDir,
    #[error("failed to create history storage directory: {0}")]
    CreateDir(String),
    #[error("database error: {0}")]
    Sqlite(String),
    #[error("history lock poisoned")]
    LockPoisoned,
}

impl From<rusqlite::Error> for QueryStoreError {
    fn from(err: rusqlite::Error) -> Self {
        Self::Sqlite(err.to_string())
    }
}

/// Local SQLite storage (`history.db`) for executed query history and saved queries.
pub struct QueryStore {
    conn: Arc<Mutex<Connection>>,
}

impl QueryStore {
    /// Resolves the default platform path (`history.db` under `ProjectDirs::data_local_dir()`).
    pub fn default_path() -> Result<PathBuf, QueryStoreError> {
        let dirs = ProjectDirs::from("dev", "tableview", "TableView").ok_or(QueryStoreError::NoDataDir)?;
        Ok(dirs.data_local_dir().join("history.db"))
    }

    /// Opens or creates the default SQLite `QueryStore` on disk.
    pub fn open_default() -> Result<Self, QueryStoreError> {
        Self::open_path(Self::default_path()?)
    }

    /// Opens or creates a SQLite `QueryStore` at the supplied file path.
    pub fn open_path(path: PathBuf) -> Result<Self, QueryStoreError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| QueryStoreError::CreateDir(e.to_string()))?;
        }
        let conn = Connection::open(path)?;
        drop(conn.pragma_update(None, "journal_mode", "WAL"));
        Self::init_with_connection(conn)
    }

    /// Opens an ephemeral in-memory `QueryStore` (for unit and headless integration tests).
    pub fn open_in_memory() -> Result<Self, QueryStoreError> {
        let conn = Connection::open_in_memory()?;
        Self::init_with_connection(conn)
    }

    fn init_with_connection(conn: Connection) -> Result<Self, QueryStoreError> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS query_history (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                connection_id TEXT,
                connection_name TEXT NOT NULL,
                database_name TEXT,
                sql TEXT NOT NULL,
                executed_at INTEGER NOT NULL,
                duration_ms INTEGER NOT NULL,
                rows_count INTEGER,
                error TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_query_history_time
                ON query_history (executed_at DESC, id DESC);
            CREATE INDEX IF NOT EXISTS idx_query_history_conn
                ON query_history (connection_id, executed_at DESC, id DESC);

            CREATE TABLE IF NOT EXISTS saved_queries (
                id TEXT PRIMARY KEY,
                connection_id TEXT,
                folder TEXT,
                name TEXT NOT NULL,
                sql TEXT NOT NULL,
                updated_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_saved_queries_name
                ON saved_queries (name COLLATE NOCASE ASC);",
        )?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Inserts one executed query into history and trims old rows beyond [`MAX_HISTORY_ROWS`].
    pub fn record_history(&self, entry: &NewHistoryEntry) -> Result<QueryHistoryEntry, QueryStoreError> {
        let executed_at = Utc::now();
        let ts_ms = executed_at.timestamp_millis();
        let conn_id_str = entry.connection_id.map(|id| id.to_string());
        let rows_i64 = entry.rows_count.map(|r| r.min(i64::MAX as u64) as i64);
        let dur_i64 = entry.duration_ms.min(i64::MAX as u64) as i64;

        let conn = self.conn.lock().map_err(|_| QueryStoreError::LockPoisoned)?;
        conn.execute(
            "INSERT INTO query_history
                (connection_id, connection_name, database_name, sql, executed_at, duration_ms, rows_count, error)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                conn_id_str,
                entry.connection_name,
                entry.database_name,
                entry.sql,
                ts_ms,
                dur_i64,
                rows_i64,
                entry.error
            ],
        )?;
        let id = conn.last_insert_rowid();

        conn.execute(
            "DELETE FROM query_history
             WHERE id NOT IN (SELECT id FROM query_history ORDER BY executed_at DESC, id DESC LIMIT ?1)",
            params![MAX_HISTORY_ROWS],
        )?;

        Ok(QueryHistoryEntry {
            id,
            connection_id: entry.connection_id,
            connection_name: entry.connection_name.clone(),
            database_name: entry.database_name.clone(),
            sql: entry.sql.clone(),
            executed_at,
            duration_ms: entry.duration_ms,
            rows_count: entry.rows_count,
            error: entry.error.clone(),
        })
    }

    /// Lists query history newest first, optionally filtered by connection ID and SQL substring.
    pub fn list_history(
        &self,
        connection_id: Option<ConnectionId>,
        search: Option<&str>,
        limit: usize,
    ) -> Result<Vec<QueryHistoryEntry>, QueryStoreError> {
        let conn = self.conn.lock().map_err(|_| QueryStoreError::LockPoisoned)?;
        let conn_filter = connection_id.map(|id| id.to_string());
        let like_pattern = search
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|q| format!("%{}%", q.to_ascii_lowercase()));
        let limit_i64 = (limit.clamp(1, MAX_HISTORY_ROWS as usize)) as i64;

        let mut stmt = conn.prepare(
            "SELECT id, connection_id, connection_name, database_name, sql, executed_at, duration_ms, rows_count, error
             FROM query_history
             WHERE (?1 IS NULL OR connection_id = ?1)
               AND (?2 IS NULL OR lower(sql) LIKE ?2 OR lower(COALESCE(database_name, '')) LIKE ?2)
             ORDER BY executed_at DESC, id DESC
             LIMIT ?3",
        )?;

        let rows = stmt.query_map(params![conn_filter, like_pattern, limit_i64], |row| {
            let cid_str: Option<String> = row.get(1)?;
            let ts_ms: i64 = row.get(5)?;
            let dur_i64: i64 = row.get(6)?;
            let rows_i64: Option<i64> = row.get(7)?;
            Ok(QueryHistoryEntry {
                id: row.get(0)?,
                connection_id: cid_str.and_then(|s| s.parse().ok()),
                connection_name: row.get(2)?,
                database_name: row.get(3)?,
                sql: row.get(4)?,
                executed_at: millis_to_utc(ts_ms),
                duration_ms: dur_i64.max(0) as u64,
                rows_count: rows_i64.map(|n| n.max(0) as u64),
                error: row.get(8)?,
            })
        })?;

        let mut out = Vec::new();
        for item in rows {
            out.push(item?);
        }
        Ok(out)
    }

    /// Clears stored query history either for one connection or globally (`None`).
    pub fn clear_history(&self, connection_id: Option<ConnectionId>) -> Result<(), QueryStoreError> {
        let conn = self.conn.lock().map_err(|_| QueryStoreError::LockPoisoned)?;
        match connection_id {
            Some(id) => {
                conn.execute("DELETE FROM query_history WHERE connection_id = ?1", params![id.to_string()])?;
            }
            None => {
                conn.execute("DELETE FROM query_history", [])?;
            }
        }
        Ok(())
    }

    /// Inserts or updates a saved SQL query in local storage.
    pub fn upsert_saved_query(&self, query: &SavedQuery) -> Result<(), QueryStoreError> {
        let conn = self.conn.lock().map_err(|_| QueryStoreError::LockPoisoned)?;
        let id_str = query.id.to_string();
        let conn_str = query.connection_id.map(|id| id.to_string());
        let folder = query
            .folder
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let ts_ms = query.updated_at.timestamp_millis();

        conn.execute(
            "INSERT INTO saved_queries (id, connection_id, folder, name, sql, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET
                connection_id = excluded.connection_id,
                folder = excluded.folder,
                name = excluded.name,
                sql = excluded.sql,
                updated_at = excluded.updated_at",
            params![id_str, conn_str, folder, query.name.trim(), query.sql, ts_ms],
        )?;
        Ok(())
    }

    /// Lists saved queries matching either `connection_id` or global (`connection_id IS NULL`),
    /// optionally filtered by substring match on name, folder, or SQL.
    pub fn list_saved_queries(
        &self,
        connection_id: Option<ConnectionId>,
        search: Option<&str>,
    ) -> Result<Vec<SavedQuery>, QueryStoreError> {
        let conn = self.conn.lock().map_err(|_| QueryStoreError::LockPoisoned)?;
        let conn_filter = connection_id.map(|id| id.to_string());
        let like_pattern = search
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|q| format!("%{}%", q.to_ascii_lowercase()));

        let mut stmt = conn.prepare(
            "SELECT id, connection_id, folder, name, sql, updated_at
             FROM saved_queries
             WHERE (?1 IS NULL OR connection_id IS NULL OR connection_id = ?1)
               AND (?2 IS NULL OR lower(name) LIKE ?2 OR lower(COALESCE(folder, '')) LIKE ?2 OR lower(sql) LIKE ?2)
             ORDER BY updated_at DESC, name COLLATE NOCASE ASC",
        )?;

        let rows = stmt.query_map(params![conn_filter, like_pattern], |row| {
            let id_str: String = row.get(0)?;
            let cid_str: Option<String> = row.get(1)?;
            let ts_ms: i64 = row.get(5)?;
            Ok(SavedQuery {
                id: id_str.parse().unwrap_or_default(),
                connection_id: cid_str.and_then(|s| s.parse().ok()),
                folder: row.get(2)?,
                name: row.get(3)?,
                sql: row.get(4)?,
                updated_at: millis_to_utc(ts_ms),
            })
        })?;

        let mut out = Vec::new();
        for item in rows {
            out.push(item?);
        }
        Ok(out)
    }

    /// Reads one saved query by ID if present.
    pub fn get_saved_query(&self, id: SavedQueryId) -> Result<Option<SavedQuery>, QueryStoreError> {
        let conn = self.conn.lock().map_err(|_| QueryStoreError::LockPoisoned)?;
        let res = conn
            .query_row(
                "SELECT id, connection_id, folder, name, sql, updated_at FROM saved_queries WHERE id = ?1",
                params![id.to_string()],
                |row| {
                    let id_str: String = row.get(0)?;
                    let cid_str: Option<String> = row.get(1)?;
                    let ts_ms: i64 = row.get(5)?;
                    Ok(SavedQuery {
                        id: id_str.parse().unwrap_or_default(),
                        connection_id: cid_str.and_then(|s| s.parse().ok()),
                        folder: row.get(2)?,
                        name: row.get(3)?,
                        sql: row.get(4)?,
                        updated_at: millis_to_utc(ts_ms),
                    })
                },
            )
            .optional()?;
        Ok(res)
    }

    /// Deletes a saved query by ID.
    pub fn delete_saved_query(&self, id: SavedQueryId) -> Result<(), QueryStoreError> {
        let conn = self.conn.lock().map_err(|_| QueryStoreError::LockPoisoned)?;
        conn.execute("DELETE FROM saved_queries WHERE id = ?1", params![id.to_string()])?;
        Ok(())
    }
}

fn millis_to_utc(ms: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(ms).single().unwrap_or_else(Utc::now)
}
