use std::collections::BTreeMap;
use std::fmt;

use crate::connection::DbEngine;
use crate::engine::{
    CellValue, ColumnMeta, ForeignKeyRef, MysqlRawValue, PostgresRawValue, RawCellValue, SqliteRawValue,
};

/// Identifier for a newly inserted unsaved row inside a table tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DraftRowId(pub u64);

impl DraftRowId {
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for DraftRowId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "new-{}", self.0)
    }
}

/// Typed pending value for an inline cell edit or draft insert cell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StagedCellValue {
    Null,
    DefaultExpr,
    Literal(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LogicalColType {
    Bool,
    Int,
    Float,
    Numeric,
    Json,
    Date,
    Time,
    Timestamp,
    TimestampTz,
    Uuid,
    Bytes,
    Array,
    Text,
}

pub(crate) fn classify_column_type(col: &ColumnMeta) -> LogicalColType {
    let udt = col.udt_name.trim().to_ascii_lowercase();
    let dtype = col.data_type.trim().to_ascii_lowercase();
    if udt.starts_with("tinyint(1)")
        || dtype.starts_with("tinyint(1)")
        || matches!(udt.as_str(), "bool" | "boolean")
        || dtype == "boolean"
    {
        return LogicalColType::Bool;
    }
    if udt.starts_with('_') {
        return LogicalColType::Array;
    }
    let base = udt.split('(').next().unwrap_or(&udt).trim();
    match base {
        "int2" | "int4" | "int8" | "integer" | "int" | "bigint" | "smallint" | "mediumint" | "tinyint" => {
            LogicalColType::Int
        }
        "float4" | "float8" | "real" | "double" | "double precision" | "float" => LogicalColType::Float,
        "numeric" | "money" | "decimal" => LogicalColType::Numeric,
        "json" | "jsonb" => LogicalColType::Json,
        "date" => LogicalColType::Date,
        "time" | "timetz" => LogicalColType::Time,
        "timestamptz" => LogicalColType::TimestampTz,
        "timestamp" | "datetime" => LogicalColType::Timestamp,
        "uuid" => LogicalColType::Uuid,
        "bytea" | "blob" | "tinyblob" | "mediumblob" | "longblob" | "varbinary" | "binary" => LogicalColType::Bytes,
        _ => LogicalColType::Text,
    }
}

impl StagedCellValue {
    /// Converts a staged cell value into a [`CellValue`] preview for rendering in the grid and inspector.
    pub fn to_preview_cell(&self, col: &ColumnMeta) -> CellValue {
        match self {
            Self::Null => CellValue::Null,
            Self::DefaultExpr => CellValue::Text("DEFAULT".to_owned()),
            Self::Literal(raw) => match classify_column_type(col) {
                LogicalColType::Bool => match raw.trim().to_ascii_lowercase().as_str() {
                    "true" | "t" | "1" | "yes" | "y" | "on" => CellValue::Bool(true),
                    "false" | "f" | "0" | "no" | "n" | "off" => CellValue::Bool(false),
                    _ => CellValue::Text(raw.clone()),
                },
                LogicalColType::Int => match raw.trim().parse::<i64>() {
                    Ok(v) => CellValue::Int(v),
                    Err(_) => CellValue::Text(raw.clone()),
                },
                LogicalColType::Float => match raw.trim().parse::<f64>() {
                    Ok(v) if v.is_finite() => CellValue::Float(v),
                    _ => CellValue::Text(raw.clone()),
                },
                LogicalColType::Numeric => CellValue::Numeric(raw.trim().to_owned()),
                LogicalColType::Json => CellValue::Json(raw.clone()),
                LogicalColType::Date => CellValue::Date(raw.clone()),
                LogicalColType::Time => CellValue::Time(raw.clone()),
                LogicalColType::TimestampTz => CellValue::parse_from_pg_text("timestamptz", Some(raw.clone())),
                LogicalColType::Timestamp => CellValue::parse_from_pg_text("timestamp", Some(raw.clone())),
                LogicalColType::Uuid => CellValue::parse_from_pg_text("uuid", Some(raw.clone())),
                LogicalColType::Bytes => {
                    let hex = strip_hex_prefix(raw.trim());
                    CellValue::parse_from_pg_text("bytea", Some(hex.to_owned()))
                }
                LogicalColType::Array => CellValue::Array(raw.clone()),
                LogicalColType::Text => CellValue::Text(raw.clone()),
            },
        }
    }

    /// Returns whether this staged value matches the original database cell without an effective change.
    pub fn matches_original(&self, orig: &CellValue) -> bool {
        match (self, orig) {
            (Self::Null, CellValue::Null) => true,
            (Self::DefaultExpr, _) => false,
            (Self::Literal(s), CellValue::Bool(b)) => match s.trim().to_ascii_lowercase().as_str() {
                "true" | "t" => *b,
                "false" | "f" => !*b,
                _ => false,
            },
            (Self::Literal(s), CellValue::Null) => {
                let _ = s;
                false
            }
            (Self::Literal(s), other) => s == &other.to_edit_string(),
            (Self::Null, _) => false,
        }
    }

    /// Formats the value concisely for line-based staged diff views.
    pub fn diff_display(&self) -> String {
        match self {
            Self::Null => "NULL".to_owned(),
            Self::DefaultExpr => "DEFAULT".to_owned(),
            Self::Literal(s) => format_diff_scalar(s),
        }
    }
}

fn format_diff_scalar(s: &str) -> String {
    if s.contains('\n') || s.contains('\r') {
        format!("{:?}", s)
    } else {
        format!("'{}'", escape_sql_string(s))
    }
}

fn format_cell_for_diff(val: &CellValue) -> String {
    match val {
        CellValue::Null => "NULL".to_owned(),
        other => format_diff_scalar(&other.copy_text()),
    }
}

/// Staged field updates against one existing fetched page row.
#[derive(Clone, Debug, PartialEq)]
pub struct StagedRowUpdate {
    pub original_row: Vec<CellValue>,
    pub original_raw_row: Option<Vec<RawCellValue>>,
    pub edits: BTreeMap<usize, StagedCellValue>,
}

/// Staged deletion targeting one existing fetched page row.
#[derive(Clone, Debug, PartialEq)]
pub struct StagedRowDelete {
    pub original_row: Vec<CellValue>,
    pub original_raw_row: Option<Vec<RawCellValue>>,
}

/// Staged unsaved insert row awaiting transactional commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StagedInsertRow {
    pub id: DraftRowId,
    pub values: Vec<StagedCellValue>,
}

/// Validation errors returned when generating SQL or bound mutations from staged table changes.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StagingError {
    #[error("table {schema}.{table} requires a primary key to update or delete existing rows safely")]
    MissingPrimaryKey { schema: String, table: String },
    #[error("cannot safely identify {schema}.{table} ({row_label}): primary key contains NULL")]
    NullPrimaryKey {
        schema: String,
        table: String,
        row_label: String,
    },
    #[error("cannot safely target {schema}.{table} ({row_label}): row contains non-UTF-8 SQLite TEXT")]
    UnbindableSqliteText {
        schema: String,
        table: String,
        row_label: String,
    },
    #[error("no staged changes to commit")]
    EmptyChanges,
}

/// Captured original value for an existing row column used in PK identity or optimistic concurrency predicates.
#[derive(Clone, Debug, PartialEq)]
pub struct StagedColumnValue {
    pub column: ColumnMeta,
    pub display: CellValue,
    pub raw: Option<RawCellValue>,
}

/// One pending column assignment inside a staged update or insert.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StagedColumnAssignment {
    pub column: ColumnMeta,
    pub value: StagedCellValue,
}

/// One bound-safe mutation inside a transactional staged commit batch.
#[derive(Clone, Debug, PartialEq)]
pub enum StagedMutation {
    Update {
        row_label: String,
        assignments: Vec<StagedColumnAssignment>,
        pk_predicates: Vec<StagedColumnValue>,
        concurrency_predicates: Vec<StagedColumnValue>,
    },
    Insert {
        row_label: String,
        assignments: Vec<StagedColumnAssignment>,
    },
    Delete {
        row_label: String,
        pk_predicates: Vec<StagedColumnValue>,
        concurrency_predicates: Vec<StagedColumnValue>,
    },
}

/// Structured batch of staged row updates, inserts, and deletes executed inside a single transaction.
#[derive(Clone, Debug, PartialEq)]
pub struct StagedCommitBatch {
    pub schema: String,
    pub table: String,
    pub mutations: Vec<StagedMutation>,
}

/// Checks whether an existing row has a reliable non-NULL primary key and bindable raw values before staging edits.
pub fn validate_row_editable(
    engine: DbEngine,
    columns: &[ColumnMeta],
    row: &[CellValue],
    raw_row: Option<&[RawCellValue]>,
) -> Result<(), String> {
    let pk_indices: Vec<usize> = columns
        .iter()
        .enumerate()
        .filter_map(|(idx, col)| col.is_primary_key.then_some(idx))
        .collect();
    if pk_indices.is_empty() {
        return Err("Table requires a primary key to edit existing rows safely".to_owned());
    }
    for &idx in &pk_indices {
        let raw_is_null = raw_row.is_some_and(|raw| raw.get(idx).is_none_or(RawCellValue::is_null));
        let display_is_null = row.get(idx).is_none_or(CellValue::is_null);
        if raw_is_null || display_is_null {
            return Err(format!(
                "Row cannot be safely edited or deleted because primary key column '{}' is NULL",
                columns[idx].name
            ));
        }
    }
    if engine == DbEngine::Sqlite
        && let Some(raw) = raw_row
        && raw.iter().any(|cell| matches!(cell, RawCellValue::Sqlite(SqliteRawValue::NonUtf8Text(_))))
    {
        return Err(
            "Row contains non-UTF-8 SQLite TEXT bytes and cannot be safely edited or deleted"
                .to_owned(),
        );
    }
    Ok(())
}

/// Pending inline cell edits, inserted draft rows, and marked-for-delete rows for one table tab.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StagedChanges {
    pub updates: BTreeMap<usize, StagedRowUpdate>,
    pub deletes: BTreeMap<usize, StagedRowDelete>,
    pub inserts: Vec<StagedInsertRow>,
    next_draft_id: u64,
}

impl StagedChanges {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.updates.is_empty() && self.deletes.is_empty() && self.inserts.is_empty()
    }

    pub fn has_changes(&self) -> bool {
        !self.is_empty()
    }

    pub fn update_count(&self) -> usize {
        self.updates.len()
    }

    pub fn insert_count(&self) -> usize {
        self.inserts.len()
    }

    pub fn delete_count(&self) -> usize {
        self.deletes.len()
    }

    /// Total number of row operations (`updated_rows + inserted_rows + deleted_rows`).
    pub fn change_count(&self) -> usize {
        self.updates.len() + self.inserts.len() + self.deletes.len()
    }

    pub fn clear(&mut self) {
        self.updates.clear();
        self.deletes.clear();
        self.inserts.clear();
    }

    /// Stages or reverts a single cell edit on an existing fetched row.
    pub fn stage_cell_edit(
        &mut self,
        row_idx: usize,
        original_row: &[CellValue],
        col_idx: usize,
        value: StagedCellValue,
    ) {
        self.stage_cell_edit_with_raw(row_idx, original_row, None, col_idx, value);
    }

    /// Stages or reverts a single cell edit on an existing fetched row while retaining its original raw wire values.
    pub fn stage_cell_edit_with_raw(
        &mut self,
        row_idx: usize,
        original_row: &[CellValue],
        original_raw_row: Option<&[RawCellValue]>,
        col_idx: usize,
        value: StagedCellValue,
    ) {
        let Some(orig_cell) = original_row.get(col_idx) else {
            return;
        };
        if value.matches_original(orig_cell) {
            if let Some(row_upd) = self.updates.get_mut(&row_idx) {
                row_upd.edits.remove(&col_idx);
                if row_upd.edits.is_empty() {
                    self.updates.remove(&row_idx);
                }
            }
            return;
        }
        let entry = self.updates.entry(row_idx).or_insert_with(|| StagedRowUpdate {
            original_row: original_row.to_vec(),
            original_raw_row: original_raw_row.map(<[RawCellValue]>::to_vec),
            edits: BTreeMap::new(),
        });
        if entry.original_raw_row.is_none()
            && let Some(raw) = original_raw_row
        {
            entry.original_raw_row = Some(raw.to_vec());
        }
        entry.edits.insert(col_idx, value);
    }

    /// Appends a new blank draft row initialized from schema defaults/nullability.
    pub fn stage_insert_row(&mut self, columns: &[ColumnMeta]) -> DraftRowId {
        self.next_draft_id = self.next_draft_id.saturating_add(1).max(1);
        let id = DraftRowId(self.next_draft_id);
        let values = columns
            .iter()
            .map(|c| {
                if c.default_expr.is_some() {
                    StagedCellValue::DefaultExpr
                } else if c.nullable {
                    StagedCellValue::Null
                } else {
                    StagedCellValue::Literal(String::new())
                }
            })
            .collect();
        self.inserts.push(StagedInsertRow { id, values });
        id
    }

    /// Updates one cell in a staged draft insert row.
    pub fn stage_insert_cell_edit(&mut self, id: DraftRowId, col_idx: usize, value: StagedCellValue) {
        if let Some(row) = self.inserts.iter_mut().find(|r| r.id == id)
            && let Some(slot) = row.values.get_mut(col_idx)
        {
            *slot = value;
        }
    }

    /// Removes a staged draft insert row by ID.
    pub fn remove_insert_row(&mut self, id: DraftRowId) -> bool {
        let before = self.inserts.len();
        self.inserts.retain(|r| r.id != id);
        self.inserts.len() != before
    }

    /// Toggles deletion status on an existing row. Marking deleted discards pending field updates on that row.
    pub fn toggle_delete_row(&mut self, row_idx: usize, original_row: &[CellValue]) -> bool {
        self.toggle_delete_row_with_raw(row_idx, original_row, None)
    }

    /// Toggles deletion status on an existing row while retaining its original raw wire values.
    pub fn toggle_delete_row_with_raw(
        &mut self,
        row_idx: usize,
        original_row: &[CellValue],
        original_raw_row: Option<&[RawCellValue]>,
    ) -> bool {
        if self.deletes.remove(&row_idx).is_some() {
            false
        } else {
            self.updates.remove(&row_idx);
            self.deletes.insert(
                row_idx,
                StagedRowDelete {
                    original_row: original_row.to_vec(),
                    original_raw_row: original_raw_row.map(<[RawCellValue]>::to_vec),
                },
            );
            true
        }
    }

    pub fn is_row_deleted(&self, row_idx: usize) -> bool {
        self.deletes.contains_key(&row_idx)
    }

    pub fn is_row_updated(&self, row_idx: usize) -> bool {
        self.updates.contains_key(&row_idx)
    }

    pub fn effective_existing_cell(&self, row_idx: usize, col_idx: usize) -> Option<&StagedCellValue> {
        self.updates.get(&row_idx).and_then(|u| u.edits.get(&col_idx))
    }

    pub fn insert_row(&self, id: DraftRowId) -> Option<&StagedInsertRow> {
        self.inserts.iter().find(|r| r.id == id)
    }

    /// Generates parameterized-safe PostgreSQL SQL statements (single transaction order: updates, inserts, deletes).
    pub fn build_statements(
        &self,
        schema: &str,
        table: &str,
        columns: &[ColumnMeta],
    ) -> Result<Vec<String>, StagingError> {
        self.build_statements_for_engine(DbEngine::Postgres, schema, table, columns)
    }

    /// Validates staged changes and builds a bound-safe commit batch with PK and optimistic concurrency checks.
    pub fn build_commit_batch(
        &self,
        engine: DbEngine,
        schema: &str,
        table: &str,
        columns: &[ColumnMeta],
    ) -> Result<StagedCommitBatch, StagingError> {
        if self.is_empty() {
            return Err(StagingError::EmptyChanges);
        }

        let pk_indices: Vec<usize> = columns
            .iter()
            .enumerate()
            .filter_map(|(idx, col)| col.is_primary_key.then_some(idx))
            .collect();

        if (!self.updates.is_empty() || !self.deletes.is_empty()) && pk_indices.is_empty() {
            return Err(StagingError::MissingPrimaryKey {
                schema: schema.to_owned(),
                table: table.to_owned(),
            });
        }

        let mut mutations = Vec::with_capacity(self.updates.len() + self.inserts.len() + self.deletes.len());

        // 1. Updates
        for (row_idx, upd) in &self.updates {
            if self.deletes.contains_key(row_idx) || upd.edits.is_empty() {
                continue;
            }
            let assignments: Vec<StagedColumnAssignment> = upd
                .edits
                .iter()
                .filter_map(|(&col_idx, val)| {
                    let col = columns.get(col_idx)?;
                    Some(StagedColumnAssignment {
                        column: col.clone(),
                        value: val.clone(),
                    })
                })
                .collect();
            if assignments.is_empty() {
                continue;
            }
            let row_label = format_row_label(*row_idx, &pk_indices, columns, &upd.original_row);
            let (pk_predicates, concurrency_predicates) = build_row_predicates(
                engine,
                schema,
                table,
                &row_label,
                &pk_indices,
                columns,
                &upd.original_row,
                upd.original_raw_row.as_deref(),
            )?;
            mutations.push(StagedMutation::Update {
                row_label,
                assignments,
                pk_predicates,
                concurrency_predicates,
            });
        }

        // 2. Inserts
        for draft in &self.inserts {
            let mut assignments = Vec::new();
            for (col_idx, col) in columns.iter().enumerate() {
                let staged_val = draft.values.get(col_idx).unwrap_or(&StagedCellValue::DefaultExpr);
                if matches!(staged_val, StagedCellValue::DefaultExpr) {
                    continue;
                }
                assignments.push(StagedColumnAssignment {
                    column: col.clone(),
                    value: staged_val.clone(),
                });
            }
            mutations.push(StagedMutation::Insert {
                row_label: format!("New Row #{}", draft.id.get()),
                assignments,
            });
        }

        // 3. Deletes
        for (row_idx, del) in &self.deletes {
            let row_label = format_row_label(*row_idx, &pk_indices, columns, &del.original_row);
            let (pk_predicates, concurrency_predicates) = build_row_predicates(
                engine,
                schema,
                table,
                &row_label,
                &pk_indices,
                columns,
                &del.original_row,
                del.original_raw_row.as_deref(),
            )?;
            mutations.push(StagedMutation::Delete {
                row_label,
                pk_predicates,
                concurrency_predicates,
            });
        }

        if mutations.is_empty() {
            return Err(StagingError::EmptyChanges);
        }

        Ok(StagedCommitBatch {
            schema: schema.to_owned(),
            table: table.to_owned(),
            mutations,
        })
    }

    /// Generates dialect-aware SQL statements (single transaction order: updates, inserts, deletes).
    pub fn build_statements_for_engine(
        &self,
        engine: DbEngine,
        schema: &str,
        table: &str,
        columns: &[ColumnMeta],
    ) -> Result<Vec<String>, StagingError> {
        self.build_commit_batch(engine, schema, table, columns)?
            .render_sql(engine)
    }

    /// Formats all staged changes as a readable `BEGIN; ... COMMIT;` SQL script.
    pub fn build_transaction_sql(
        &self,
        schema: &str,
        table: &str,
        columns: &[ColumnMeta],
    ) -> Result<String, StagingError> {
        self.build_transaction_sql_for_engine(DbEngine::Postgres, schema, table, columns)
    }

    /// Formats all staged changes as a dialect-appropriate transaction SQL script.
    pub fn build_transaction_sql_for_engine(
        &self,
        engine: DbEngine,
        schema: &str,
        table: &str,
        columns: &[ColumnMeta],
    ) -> Result<String, StagingError> {
        let stmts = self.build_statements_for_engine(engine, schema, table, columns)?;
        let begin = match engine {
            DbEngine::Mysql => "START TRANSACTION;",
            DbEngine::Postgres | DbEngine::Sqlite => "BEGIN;",
        };
        Ok(format!("{begin}\n\n{}\n\nCOMMIT;\n", stmts.join("\n")))
    }

    /// Builds synchronized before/after text blocks so `rune_kit::diff::FileDiff` renders row-level changes clearly.
    pub fn build_diff_texts(
        &self,
        schema: &str,
        table: &str,
        columns: &[ColumnMeta],
    ) -> (String, String) {
        let pk_indices: Vec<usize> = columns
            .iter()
            .enumerate()
            .filter_map(|(idx, col)| col.is_primary_key.then_some(idx))
            .collect();

        let mut before = String::new();
        let mut after = String::new();

        for (row_idx, upd) in &self.updates {
            if self.deletes.contains_key(row_idx) || upd.edits.is_empty() {
                continue;
            }
            let row_tag = format_row_label(*row_idx, &pk_indices, columns, &upd.original_row);
            before.push_str(&format!("-- {schema}.{table} {row_tag}\n"));
            after.push_str(&format!("-- {schema}.{table} {row_tag} [UPDATE]\n"));

            for (col_idx, col) in columns.iter().enumerate() {
                let orig = upd.original_row.get(col_idx).unwrap_or(&CellValue::Null);
                let orig_s = format_cell_for_diff(orig);
                let new_s = upd
                    .edits
                    .get(&col_idx)
                    .map_or_else(|| orig_s.clone(), StagedCellValue::diff_display);
                before.push_str(&format!("  {} = {orig_s}\n", col.name));
                after.push_str(&format!("  {} = {new_s}\n", col.name));
            }
            before.push('\n');
            after.push('\n');
        }

        for draft in &self.inserts {
            after.push_str(&format!("-- {schema}.{table} New Row #{} [INSERT]\n", draft.id.get()));
            for (col_idx, col) in columns.iter().enumerate() {
                let val = draft.values.get(col_idx).unwrap_or(&StagedCellValue::DefaultExpr);
                after.push_str(&format!("  {} = {}\n", col.name, val.diff_display()));
            }
            after.push('\n');
        }

        for (row_idx, del) in &self.deletes {
            let row_label = format_row_label(*row_idx, &pk_indices, columns, &del.original_row);
            before.push_str(&format!("-- {schema}.{table} {row_label} [DELETE]\n"));
            for (col_idx, col) in columns.iter().enumerate() {
                let orig = del.original_row.get(col_idx).unwrap_or(&CellValue::Null);
                before.push_str(&format!("  {} = {}\n", col.name, format_cell_for_diff(orig)));
            }
            before.push('\n');
        }

        (before, after)
    }
}

fn format_row_label(
    row_idx: usize,
    pk_indices: &[usize],
    columns: &[ColumnMeta],
    row: &[CellValue],
) -> String {
    if pk_indices.is_empty() {
        return format!("row #{}", row_idx + 1);
    }
    let parts: Vec<String> = pk_indices
        .iter()
        .filter_map(|&idx| {
            let col = columns.get(idx)?;
            let val = row.get(idx).unwrap_or(&CellValue::Null);
            Some(format!("{}={}", col.name, val.copy_text()))
        })
        .collect();
    parts.join(", ")
}

#[allow(clippy::too_many_arguments)]
fn build_row_predicates(
    engine: DbEngine,
    schema: &str,
    table: &str,
    row_label: &str,
    pk_indices: &[usize],
    columns: &[ColumnMeta],
    row: &[CellValue],
    raw_row: Option<&[RawCellValue]>,
) -> Result<(Vec<StagedColumnValue>, Vec<StagedColumnValue>), StagingError> {
    let mut pk_predicates = Vec::with_capacity(pk_indices.len());
    for &idx in pk_indices {
        let Some(col) = columns.get(idx) else {
            continue;
        };
        let raw = raw_row.and_then(|r| r.get(idx)).cloned();
        let display = row.get(idx).cloned().unwrap_or(CellValue::Null);
        if raw.as_ref().is_some_and(RawCellValue::is_null) || display.is_null() {
            return Err(StagingError::NullPrimaryKey {
                schema: schema.to_owned(),
                table: table.to_owned(),
                row_label: row_label.to_owned(),
            });
        }
        if engine == DbEngine::Sqlite
            && matches!(raw, Some(RawCellValue::Sqlite(SqliteRawValue::NonUtf8Text(_))))
        {
            return Err(StagingError::UnbindableSqliteText {
                schema: schema.to_owned(),
                table: table.to_owned(),
                row_label: row_label.to_owned(),
            });
        }
        pk_predicates.push(StagedColumnValue {
            column: col.clone(),
            display,
            raw,
        });
    }

    let mut concurrency_predicates = Vec::with_capacity(columns.len().saturating_sub(pk_indices.len()));
    for (idx, col) in columns.iter().enumerate() {
        if col.is_primary_key {
            continue;
        }
        let raw = raw_row.and_then(|r| r.get(idx)).cloned();
        let display = row.get(idx).cloned().unwrap_or(CellValue::Null);
        if engine == DbEngine::Sqlite
            && matches!(raw, Some(RawCellValue::Sqlite(SqliteRawValue::NonUtf8Text(_))))
        {
            return Err(StagingError::UnbindableSqliteText {
                schema: schema.to_owned(),
                table: table.to_owned(),
                row_label: row_label.to_owned(),
            });
        }
        concurrency_predicates.push(StagedColumnValue {
            column: col.clone(),
            display,
            raw,
        });
    }

    Ok((pk_predicates, concurrency_predicates))
}

impl StagedCommitBatch {
    /// Renders SQL statements for display in diffs / copy-SQL or generic fallback execution.
    pub fn render_sql(&self, engine: DbEngine) -> Result<Vec<String>, StagingError> {
        if self.mutations.is_empty() {
            return Err(StagingError::EmptyChanges);
        }
        let fq_table = engine.qualify_ident(&self.schema, &self.table);
        let mut stmts = Vec::with_capacity(self.mutations.len());

        for mutation in &self.mutations {
            match mutation {
                StagedMutation::Update {
                    row_label,
                    assignments,
                    pk_predicates,
                    concurrency_predicates,
                } => {
                    let set_clauses: Vec<String> = assignments
                        .iter()
                        .map(|a| {
                            format!(
                                "{} = {}",
                                engine.quote_ident(&a.column.name),
                                format_literal(engine, &a.column, &a.value)
                            )
                        })
                        .collect();
                    let where_sql = render_predicates_where(
                        engine,
                        &self.schema,
                        &self.table,
                        row_label,
                        pk_predicates,
                        concurrency_predicates,
                    )?;
                    stmts.push(format!(
                        "UPDATE {fq_table} SET {} WHERE {where_sql};",
                        set_clauses.join(", ")
                    ));
                }
                StagedMutation::Insert { assignments, .. } => {
                    if assignments.is_empty() {
                        match engine {
                            DbEngine::Mysql => stmts.push(format!("INSERT INTO {fq_table} () VALUES ();")),
                            DbEngine::Postgres | DbEngine::Sqlite => {
                                stmts.push(format!("INSERT INTO {fq_table} DEFAULT VALUES;"));
                            }
                        }
                    } else {
                        let col_names: Vec<String> = assignments
                            .iter()
                            .map(|a| engine.quote_ident(&a.column.name))
                            .collect();
                        let val_exprs: Vec<String> = assignments
                            .iter()
                            .map(|a| format_literal(engine, &a.column, &a.value))
                            .collect();
                        stmts.push(format!(
                            "INSERT INTO {fq_table} ({}) VALUES ({});",
                            col_names.join(", "),
                            val_exprs.join(", ")
                        ));
                    }
                }
                StagedMutation::Delete {
                    row_label,
                    pk_predicates,
                    concurrency_predicates,
                } => {
                    let where_sql = render_predicates_where(
                        engine,
                        &self.schema,
                        &self.table,
                        row_label,
                        pk_predicates,
                        concurrency_predicates,
                    )?;
                    stmts.push(format!("DELETE FROM {fq_table} WHERE {where_sql};"));
                }
            }
        }

        Ok(stmts)
    }
}

fn render_predicates_where(
    engine: DbEngine,
    schema: &str,
    table: &str,
    row_label: &str,
    pk_predicates: &[StagedColumnValue],
    concurrency_predicates: &[StagedColumnValue],
) -> Result<String, StagingError> {
    if pk_predicates.is_empty() {
        return Err(StagingError::MissingPrimaryKey {
            schema: schema.to_owned(),
            table: table.to_owned(),
        });
    }
    let mut clauses = Vec::with_capacity(pk_predicates.len() + concurrency_predicates.len());
    for pk in pk_predicates {
        if pk.raw.as_ref().is_some_and(RawCellValue::is_null) || pk.display.is_null() {
            return Err(StagingError::NullPrimaryKey {
                schema: schema.to_owned(),
                table: table.to_owned(),
                row_label: row_label.to_owned(),
            });
        }
        clauses.push(render_predicate_clause(engine, pk, true));
    }
    for col in concurrency_predicates {
        clauses.push(render_predicate_clause(engine, col, false));
    }
    Ok(clauses.join(" AND "))
}

pub(crate) fn is_mysql_text_col(col: &ColumnMeta) -> bool {
    matches!(
        col.udt_name.trim().to_ascii_lowercase().as_str(),
        "char" | "varchar" | "tinytext" | "text" | "mediumtext" | "longtext" | "enum" | "set"
    )
}

fn render_predicate_clause(engine: DbEngine, val: &StagedColumnValue, is_pk: bool) -> String {
    let quote = engine.quote_ident(&val.column.name);
    if val.raw.as_ref().is_some_and(RawCellValue::is_null) || val.display.is_null() {
        return format!("{quote} IS NULL");
    }
    match engine {
        DbEngine::Postgres => {
            let raw_text = match &val.raw {
                Some(RawCellValue::Postgres(crate::engine::PostgresRawValue::Text(s))) => s.clone(),
                _ => val.display.copy_text(),
            };
            if is_pk || val.column.udt_name == "bytea" {
                let base = format!("{quote} = {}", format_pg_cell_literal(&val.column, &val.display));
                if is_pk
                    && matches!(
                        val.column.udt_name.as_str(),
                        "text" | "varchar" | "bpchar" | "name" | "citext"
                    )
                {
                    format!(
                        "{base} AND {quote}::text COLLATE \"C\" = '{}' COLLATE \"C\"",
                        escape_sql_string(&raw_text)
                    )
                } else {
                    base
                }
            } else {
                format!(
                    "{quote}::text COLLATE \"C\" = '{}' COLLATE \"C\"",
                    escape_sql_string(&raw_text)
                )
            }
        }
        DbEngine::Mysql => {
            let lit = format_mysql_cell_literal(&val.display);
            if classify_column_type(&val.column) == LogicalColType::Json {
                format!("{quote} = CAST({lit} AS JSON)")
            } else if is_mysql_text_col(&val.column) {
                let exact_eq = format!(
                    "CAST(CONVERT({quote} USING utf8mb4) AS BINARY) = CAST(CONVERT({lit} USING utf8mb4) AS BINARY)"
                );
                if is_pk {
                    format!("{quote} = {lit} AND {exact_eq}")
                } else {
                    exact_eq
                }
            } else {
                format!("{quote} = {lit}")
            }
        }
        DbEngine::Sqlite => {
            if let Some(RawCellValue::Sqlite(raw)) = &val.raw {
                let lhs = if matches!(raw, SqliteRawValue::Text(_) | SqliteRawValue::NonUtf8Text(_)) {
                    format!("{quote} COLLATE BINARY")
                } else {
                    quote.clone()
                };
                format!(
                    "{lhs} = {} AND typeof({quote}) = '{}'",
                    format_sqlite_raw_literal(raw),
                    raw.typeof_name()
                )
            } else {
                let lit = format_sqlite_cell_literal(&val.display);
                if lit.starts_with('\'') || lit.starts_with("char(") {
                    format!("{quote} COLLATE BINARY = {lit}")
                } else {
                    format!("{quote} = {lit}")
                }
            }
        }
    }
}

fn format_sqlite_string_literal(raw: &str) -> String {
    if !raw.contains('\0') {
        return format!("'{}'", escape_sql_string(raw));
    }
    let mut parts = Vec::new();
    let mut i = 0usize;
    let bytes = raw.as_bytes();
    while i < bytes.len() {
        if bytes[i] == 0 {
            let start = i;
            while i < bytes.len() && bytes[i] == 0 {
                i += 1;
            }
            let zeros = std::iter::repeat_n("0", i - start).collect::<Vec<_>>().join(", ");
            parts.push(format!("char({zeros})"));
        } else {
            let start = i;
            while i < bytes.len() && bytes[i] != 0 {
                i += 1;
            }
            parts.push(format!("'{}'", escape_sql_string(&raw[start..i])));
        }
    }
    parts.join(" || ")
}

fn format_sqlite_raw_literal(raw: &SqliteRawValue) -> String {
    match raw {
        SqliteRawValue::Null => "NULL".to_owned(),
        SqliteRawValue::Integer(i) => i.to_string(),
        SqliteRawValue::Real(f) => {
            if f.is_finite() {
                let s = f.to_string();
                if s.contains(['.', 'e', 'E']) {
                    s
                } else {
                    format!("{s}.0")
                }
            } else {
                format_sqlite_string_literal(&f.to_string())
            }
        }
        SqliteRawValue::Text(s) => format_sqlite_string_literal(s),
        SqliteRawValue::NonUtf8Text(bytes) => {
            format!("CAST(X'{}' AS TEXT)", crate::engine::encode_hex(bytes))
        }
        SqliteRawValue::Blob(bytes) => format!("X'{}'", crate::engine::encode_hex(bytes)),
    }
}

/// Escapes single quotes inside SQL string literals by doubling `'`.
pub fn escape_sql_string(raw: &str) -> String {
    raw.replace('\'', "''")
}

/// Escapes backslashes and single quotes for MySQL string literals (`\` -> `\\`, `'` -> `''`).
pub fn escape_mysql_string(raw: &str) -> String {
    raw.replace('\\', "\\\\").replace('\'', "''")
}

/// Formats a MySQL text literal so embedded backslashes evaluate identically regardless of `NO_BACKSLASH_ESCAPES`.
pub fn format_mysql_string_literal(raw: &str) -> String {
    if !raw.contains('\\') {
        return format!("'{}'", escape_sql_string(raw));
    }
    let bytes = raw.as_bytes();
    let mut parts = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            let start = i;
            while i < bytes.len() && bytes[i] == b'\\' {
                i += 1;
            }
            let count = i - start;
            match count {
                1 => parts.push("CHAR(92 USING utf8mb4)".to_owned()),
                2 => parts.push("CHAR(92, 92 USING utf8mb4)".to_owned()),
                n => parts.push(format!("REPEAT(CHAR(92 USING utf8mb4), {n})")),
            }
        } else {
            let start = i;
            while i < bytes.len() && bytes[i] != b'\\' {
                i += 1;
            }
            let seg = &raw[start..i];
            parts.push(format!("'{}'", escape_sql_string(seg)));
        }
    }
    if parts.len() == 1 {
        parts.pop().unwrap_or_else(|| "''".to_owned())
    } else {
        format!("CONCAT({})", parts.join(", "))
    }
}

pub(crate) fn strip_hex_prefix(raw: &str) -> &str {
    raw.strip_prefix("\\x")
        .or_else(|| raw.strip_prefix("\\X"))
        .or_else(|| raw.strip_prefix("0x"))
        .or_else(|| raw.strip_prefix("0X"))
        .unwrap_or(raw)
}

/// Formats a staged cell edit into a dialect-safe SQL literal or expression.
pub fn format_literal(engine: DbEngine, col: &ColumnMeta, val: &StagedCellValue) -> String {
    match val {
        StagedCellValue::Null => "NULL".to_owned(),
        StagedCellValue::DefaultExpr => "DEFAULT".to_owned(),
        StagedCellValue::Literal(raw) => match engine {
            DbEngine::Postgres => format_raw_pg_literal(col, raw),
            DbEngine::Mysql => format_raw_mysql_literal(col, raw),
            DbEngine::Sqlite => format_raw_sqlite_literal(col, raw),
        },
    }
}

/// Formats an existing decoded [`CellValue`] into a dialect-safe SQL literal.
pub fn format_cell_literal(engine: DbEngine, col: &ColumnMeta, val: &CellValue) -> String {
    match engine {
        DbEngine::Postgres => format_pg_cell_literal(col, val),
        DbEngine::Mysql => format_mysql_cell_literal(val),
        DbEngine::Sqlite => format_sqlite_cell_literal(val),
    }
}

/// Formats a staged cell edit into a type-safe PostgreSQL literal or expression.
pub fn format_pg_literal(col: &ColumnMeta, val: &StagedCellValue) -> String {
    format_literal(DbEngine::Postgres, col, val)
}

/// Formats an existing decoded [`CellValue`] into a PostgreSQL SQL literal for `WHERE`, `INSERT`, or FK queries.
pub fn format_pg_cell_literal(col: &ColumnMeta, val: &CellValue) -> String {
    match val {
        CellValue::Null => "NULL".to_owned(),
        CellValue::Bool(b) => if *b { "TRUE" } else { "FALSE" }.to_owned(),
        CellValue::Int(i) => i.to_string(),
        CellValue::Float(f) => {
            if f.is_finite() {
                f.to_string()
            } else {
                format!("'{}'::double precision", escape_sql_string(&f.to_string()))
            }
        }
        CellValue::Numeric(s) => format_numeric_literal(s),
        CellValue::Uuid(u) => format!("'{}'::uuid", u),
        CellValue::Timestamp { raw, .. } => {
            let cast = if col.udt_name == "timestamp" { "timestamp" } else { "timestamptz" };
            format!("'{}'::{cast}", escape_sql_string(raw))
        }
        CellValue::Date(s) => format!("'{}'::date", escape_sql_string(s)),
        CellValue::Time(s) => {
            let cast = if col.udt_name == "timetz" { "timetz" } else { "time" };
            format!("'{}'::{cast}", escape_sql_string(s))
        }
        CellValue::Json(s) => format!("'{}'::jsonb", escape_sql_string(s)),
        CellValue::Bytes(bytes) => {
            format!("'\\x{}'::bytea", crate::engine::encode_hex(bytes))
        }
        CellValue::Array(s) => {
            let cast = if col.data_type.ends_with("[]") {
                col.data_type.as_str()
            } else {
                "text[]"
            };
            format!("'{}'::{cast}", escape_sql_string(s))
        }
        CellValue::Text(s) => format!("'{}'", escape_sql_string(s)),
    }
}

fn format_mysql_cell_literal(val: &CellValue) -> String {
    match val {
        CellValue::Null => "NULL".to_owned(),
        CellValue::Bool(b) => if *b { "TRUE" } else { "FALSE" }.to_owned(),
        CellValue::Int(i) => i.to_string(),
        CellValue::Float(f) => {
            if f.is_finite() {
                f.to_string()
            } else {
                format_mysql_string_literal(&f.to_string())
            }
        }
        CellValue::Numeric(s) => {
            let trimmed = s.trim();
            if is_plain_decimal(trimmed) {
                trimmed.to_owned()
            } else {
                format_mysql_string_literal(trimmed)
            }
        }
        CellValue::Uuid(u) => format!("'{u}'"),
        CellValue::Timestamp { raw, .. }
        | CellValue::Date(raw)
        | CellValue::Time(raw)
        | CellValue::Json(raw)
        | CellValue::Array(raw)
        | CellValue::Text(raw) => format_mysql_string_literal(raw),
        CellValue::Bytes(bytes) => format!("X'{}'", crate::engine::encode_hex(bytes)),
    }
}

fn format_sqlite_cell_literal(val: &CellValue) -> String {
    match val {
        CellValue::Null => "NULL".to_owned(),
        CellValue::Bool(b) => if *b { "1" } else { "0" }.to_owned(),
        CellValue::Int(i) => i.to_string(),
        CellValue::Float(f) => {
            if f.is_finite() {
                f.to_string()
            } else {
                format_sqlite_string_literal(&f.to_string())
            }
        }
        CellValue::Numeric(s) => {
            let trimmed = s.trim();
            if is_plain_decimal(trimmed) {
                trimmed.to_owned()
            } else {
                format_sqlite_string_literal(trimmed)
            }
        }
        CellValue::Uuid(u) => format!("'{u}'"),
        CellValue::Timestamp { raw, .. }
        | CellValue::Date(raw)
        | CellValue::Time(raw)
        | CellValue::Json(raw)
        | CellValue::Array(raw)
        | CellValue::Text(raw) => format_sqlite_string_literal(raw),
        CellValue::Bytes(bytes) => format!("X'{}'", crate::engine::encode_hex(bytes)),
    }
}

fn format_raw_mysql_literal(col: &ColumnMeta, raw: &str) -> String {
    let trimmed = raw.trim();
    match classify_column_type(col) {
        LogicalColType::Bool => match trimmed.to_ascii_lowercase().as_str() {
            "true" | "t" | "1" | "yes" | "y" | "on" => "TRUE".to_owned(),
            "false" | "f" | "0" | "no" | "n" | "off" => "FALSE".to_owned(),
            _ => format_mysql_string_literal(trimmed),
        },
        LogicalColType::Int => {
            if trimmed.parse::<i64>().is_ok() {
                trimmed.to_owned()
            } else {
                format_mysql_string_literal(trimmed)
            }
        }
        LogicalColType::Float => {
            if trimmed.parse::<f64>().is_ok_and(f64::is_finite) {
                trimmed.to_owned()
            } else {
                format_mysql_string_literal(trimmed)
            }
        }
        LogicalColType::Numeric => {
            if is_plain_decimal(trimmed) {
                trimmed.to_owned()
            } else {
                format_mysql_string_literal(trimmed)
            }
        }
        LogicalColType::Bytes => {
            let hex = strip_hex_prefix(trimmed);
            if hex.len().is_multiple_of(2) && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                format!("X'{hex}'")
            } else {
                format_mysql_string_literal(raw)
            }
        }
        LogicalColType::Json | LogicalColType::Text | LogicalColType::Array => {
            format_mysql_string_literal(raw)
        }
        LogicalColType::Date
        | LogicalColType::Time
        | LogicalColType::Timestamp
        | LogicalColType::TimestampTz
        | LogicalColType::Uuid => format_mysql_string_literal(trimmed),
    }
}

fn format_raw_sqlite_literal(col: &ColumnMeta, raw: &str) -> String {
    let trimmed = raw.trim();
    match classify_column_type(col) {
        LogicalColType::Bool => match trimmed.to_ascii_lowercase().as_str() {
            "true" | "t" | "1" | "yes" | "y" | "on" => "1".to_owned(),
            "false" | "f" | "0" | "no" | "n" | "off" => "0".to_owned(),
            _ => format_sqlite_string_literal(trimmed),
        },
        LogicalColType::Int => {
            if trimmed.parse::<i64>().is_ok() {
                trimmed.to_owned()
            } else {
                format_sqlite_string_literal(trimmed)
            }
        }
        LogicalColType::Float => {
            if trimmed.parse::<f64>().is_ok_and(f64::is_finite) {
                trimmed.to_owned()
            } else {
                format_sqlite_string_literal(trimmed)
            }
        }
        LogicalColType::Numeric => {
            if is_plain_decimal(trimmed) {
                trimmed.to_owned()
            } else {
                format_sqlite_string_literal(trimmed)
            }
        }
        LogicalColType::Bytes => {
            let hex = strip_hex_prefix(trimmed);
            if hex.len().is_multiple_of(2) && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                format!("X'{hex}'")
            } else {
                format_sqlite_string_literal(raw)
            }
        }
        LogicalColType::Json | LogicalColType::Text | LogicalColType::Array => {
            format_sqlite_string_literal(raw)
        }
        LogicalColType::Date
        | LogicalColType::Time
        | LogicalColType::Timestamp
        | LogicalColType::TimestampTz
        | LogicalColType::Uuid => format_sqlite_string_literal(trimmed),
    }
}

fn format_raw_pg_literal(col: &ColumnMeta, raw: &str) -> String {
    let trimmed = raw.trim();
    match col.udt_name.as_str() {
        "bool" => match trimmed.to_ascii_lowercase().as_str() {
            "true" | "t" | "1" | "yes" | "y" | "on" => "TRUE".to_owned(),
            "false" | "f" | "0" | "no" | "n" | "off" => "FALSE".to_owned(),
            _ => format!("'{}'::boolean", escape_sql_string(trimmed)),
        },
        "int2" | "int4" | "int8" => {
            if trimmed.parse::<i64>().is_ok() {
                trimmed.to_owned()
            } else {
                format!("'{}'::{}", escape_sql_string(trimmed), col.udt_name)
            }
        }
        "float4" | "float8" => {
            if trimmed.parse::<f64>().is_ok_and(f64::is_finite) {
                trimmed.to_owned()
            } else {
                format!("'{}'::double precision", escape_sql_string(trimmed))
            }
        }
        "numeric" | "money" => format_numeric_literal(trimmed),
        "bytea" => {
            let body = if trimmed.starts_with("\\x") || trimmed.starts_with("\\X") {
                trimmed.to_owned()
            } else {
                format!("\\x{trimmed}")
            };
            format!("'{}'::bytea", escape_sql_string(&body))
        }
        "json" => format!("'{}'::json", escape_sql_string(raw)),
        "jsonb" => format!("'{}'::jsonb", escape_sql_string(raw)),
        "uuid" => format!("'{}'::uuid", escape_sql_string(trimmed)),
        "date" | "time" | "timetz" | "timestamp" | "timestamptz" => {
            format!("'{}'::{}", escape_sql_string(trimmed), col.udt_name)
        }
        _ if col.udt_name.starts_with('_') => {
            let cast = if col.data_type.ends_with("[]") {
                col.data_type.as_str()
            } else {
                "text[]"
            };
            format!("'{}'::{cast}", escape_sql_string(raw))
        }
        _ => format!("'{}'", escape_sql_string(raw)),
    }
}

fn format_numeric_literal(s: &str) -> String {
    let trimmed = s.trim();
    if is_plain_decimal(trimmed) {
        trimmed.to_owned()
    } else {
        format!("'{}'::numeric", escape_sql_string(trimmed))
    }
}

fn is_plain_decimal(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let body = s.strip_prefix(['+', '-']).unwrap_or(s);
    if body.is_empty() {
        return false;
    }
    let mut seen_dot = false;
    let mut seen_digit = false;
    for b in body.bytes() {
        match b {
            b'0'..=b'9' => seen_digit = true,
            b'.' if !seen_dot => seen_dot = true,
            _ => return false,
        }
    }
    seen_digit
}

/// Resolves the full ordered foreign-key lookup components for `fk` from `row`,
/// returning `None` when the constraint is unresolvable or any key component is `NULL` or missing.
pub fn resolve_fk_lookup_components(
    fk: &ForeignKeyRef,
    columns: &[crate::engine::ColumnMeta],
    row: &[CellValue],
) -> Option<Vec<crate::engine::FkLookupComponent>> {
    resolve_fk_lookup_components_with_raw(fk, columns, row, None)
}

/// Resolves the full ordered foreign-key lookup components for `fk` from `row` and optional
/// unedited per-cell raw values, returning `None` when unresolvable or any key component is `NULL`.
pub fn resolve_fk_lookup_components_with_raw(
    fk: &ForeignKeyRef,
    columns: &[crate::engine::ColumnMeta],
    row: &[CellValue],
    raw_row: Option<&[Option<RawCellValue>]>,
) -> Option<Vec<crate::engine::FkLookupComponent>> {
    if !fk.is_resolvable() {
        return None;
    }
    let mut components = Vec::with_capacity(fk.source_columns.len());
    for (src_col, dst_col) in fk.source_columns.iter().zip(fk.target_columns.iter()) {
        let col_idx = columns.iter().position(|c| c.name == *src_col)?;
        let val = row.get(col_idx)?;
        if val.is_null() {
            return None;
        }
        let raw_value = raw_row
            .and_then(|r| r.get(col_idx))
            .and_then(Clone::clone);
        if raw_value.as_ref().is_some_and(RawCellValue::is_null) {
            return None;
        }
        components.push(crate::engine::FkLookupComponent {
            target_column: dst_col.clone(),
            source_col: columns[col_idx].clone(),
            value: val.clone(),
            raw_value,
        });
    }
    Some(components)
}

/// Formats ordered foreign-key lookup predicates for display in badges and peek headers.
pub fn format_fk_lookup_display(components: &[crate::engine::FkLookupComponent]) -> String {
    components
        .iter()
        .map(|c| {
            let val_str = match &c.raw_value {
                Some(RawCellValue::Sqlite(SqliteRawValue::Text(s))) => s.clone(),
                _ => c.value.display_compact(),
            };
            format!("{} = {val_str}", c.target_column)
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn format_raw_fk_literal(engine: DbEngine, col: &ColumnMeta, raw: &RawCellValue) -> Option<String> {
    match (engine, raw) {
        (DbEngine::Sqlite, RawCellValue::Sqlite(v)) => match v {
            SqliteRawValue::Null => None,
            SqliteRawValue::Integer(i) => Some(i.to_string()),
            SqliteRawValue::Real(f) => {
                if f.is_finite() {
                    let mut s = f.to_string();
                    if !s.contains('.') && !s.contains('e') && !s.contains('E') {
                        s.push_str(".0");
                    }
                    Some(s)
                } else if f.is_infinite() {
                    Some(if f.is_sign_positive() {
                        "1e999".to_owned()
                    } else {
                        "-1e999".to_owned()
                    })
                } else {
                    None
                }
            }
            SqliteRawValue::Text(s) => Some(format_sqlite_string_literal(s)),
            SqliteRawValue::NonUtf8Text(bytes) => {
                Some(format!("CAST(X'{}' AS TEXT)", crate::engine::encode_hex(bytes)))
            }
            SqliteRawValue::Blob(bytes) => Some(format!("X'{}'", crate::engine::encode_hex(bytes))),
        },
        (DbEngine::Postgres, RawCellValue::Postgres(v)) => match v {
            PostgresRawValue::Null => None,
            PostgresRawValue::Bytes(bytes) => Some(format!("'\\x{}'::bytea", crate::engine::encode_hex(bytes))),
            PostgresRawValue::Text(s) => {
                if col.udt_name == "money" {
                    Some(format!("'{}'::money", escape_sql_string(s.trim())))
                } else {
                    Some(format_raw_pg_literal(col, s))
                }
            }
        },
        (DbEngine::Mysql, RawCellValue::Mysql(v)) => match v {
            MysqlRawValue::Null => None,
            MysqlRawValue::Int(i) => Some(i.to_string()),
            MysqlRawValue::UInt(u) => Some(u.to_string()),
            MysqlRawValue::Float(f) => f.is_finite().then(|| f.to_string()),
            MysqlRawValue::Double(d) => d.is_finite().then(|| d.to_string()),
            MysqlRawValue::Bytes(bytes) => {
                if classify_column_type(col) == LogicalColType::Bytes {
                    return Some(format!("X'{}'", crate::engine::encode_hex(bytes)));
                }
                let Ok(s) = std::str::from_utf8(bytes) else {
                    return Some(format!("X'{}'", crate::engine::encode_hex(bytes)));
                };
                let trimmed = s.trim();
                Some(match classify_column_type(col) {
                    LogicalColType::Bool => {
                        if trimmed.parse::<i64>().is_ok() {
                            trimmed.to_owned()
                        } else {
                            format_mysql_string_literal(s)
                        }
                    }
                    LogicalColType::Int => {
                        if trimmed.parse::<i64>().is_ok() || trimmed.parse::<u64>().is_ok() {
                            trimmed.to_owned()
                        } else {
                            format_mysql_string_literal(s)
                        }
                    }
                    LogicalColType::Float => {
                        if trimmed.parse::<f64>().is_ok_and(f64::is_finite) {
                            trimmed.to_owned()
                        } else {
                            format_mysql_string_literal(s)
                        }
                    }
                    LogicalColType::Numeric => {
                        if is_plain_decimal(trimmed) {
                            trimmed.to_owned()
                        } else {
                            format_mysql_string_literal(s)
                        }
                    }
                    LogicalColType::Text
                    | LogicalColType::Json
                    | LogicalColType::Date
                    | LogicalColType::Time
                    | LogicalColType::Timestamp
                    | LogicalColType::TimestampTz
                    | LogicalColType::Uuid
                    | LogicalColType::Array => format_mysql_string_literal(s),
                    LogicalColType::Bytes => format!("X'{}'", crate::engine::encode_hex(bytes)),
                })
            }
            MysqlRawValue::Date {
                year,
                month,
                day,
                hour,
                minute,
                second,
                micros,
            } => {
                if classify_column_type(col) == LogicalColType::Date
                    && *hour == 0
                    && *minute == 0
                    && *second == 0
                    && *micros == 0
                {
                    Some(format!("'{year:04}-{month:02}-{day:02}'"))
                } else if *micros > 0 {
                    Some(format!(
                        "'{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}.{micros:06}'"
                    ))
                } else {
                    Some(format!(
                        "'{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}'"
                    ))
                }
            }
            MysqlRawValue::Time {
                neg,
                days,
                hour,
                minute,
                second,
                micros,
            } => {
                let total_hours = days.saturating_mul(24).saturating_add(u32::from(*hour));
                let sign = if *neg { "-" } else { "" };
                if *micros > 0 {
                    Some(format!("'{sign}{total_hours:02}:{minute:02}:{second:02}.{micros:06}'"))
                } else {
                    Some(format!("'{sign}{total_hours:02}:{minute:02}:{second:02}'"))
                }
            }
        },
        _ => None,
    }
}

/// Builds a `WHERE` filter snippet targeting the referenced foreign-key row, returning `None`
/// if `fk` is unresolvable or any component is `NULL` or mismatched.
pub fn build_fk_where_clause(
    fk: &ForeignKeyRef,
    components: &[crate::engine::FkLookupComponent],
) -> Option<String> {
    build_fk_where_clause_for_engine(DbEngine::Postgres, fk, components)
}

/// Builds a dialect-aware `WHERE` filter snippet targeting the referenced foreign-key row, returning
/// `None` if `fk` is unresolvable or any component is `NULL` or mismatched.
pub fn build_fk_where_clause_for_engine(
    engine: DbEngine,
    fk: &ForeignKeyRef,
    components: &[crate::engine::FkLookupComponent],
) -> Option<String> {
    if !fk.is_resolvable() || components.len() != fk.target_columns.len() {
        return None;
    }
    let mut clauses = Vec::with_capacity(components.len());
    for (comp, expected_target) in components.iter().zip(fk.target_columns.iter()) {
        if comp.target_column != *expected_target || comp.value.is_null() {
            return None;
        }
        let literal = match &comp.raw_value {
            Some(raw) => match (engine, raw) {
                (DbEngine::Sqlite, RawCellValue::Sqlite(_))
                | (DbEngine::Postgres, RawCellValue::Postgres(_))
                | (DbEngine::Mysql, RawCellValue::Mysql(_)) => format_raw_fk_literal(engine, &comp.source_col, raw)?,
                _ => format_cell_literal(engine, &comp.source_col, &comp.value),
            },
            None => format_cell_literal(engine, &comp.source_col, &comp.value),
        };
        clauses.push(format!("{} = {literal}", engine.quote_ident(&comp.target_column)));
    }
    Some(clauses.join(" AND "))
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn col(name: &str, udt_name: &str, pk: bool, default_expr: Option<&str>) -> ColumnMeta {
        ColumnMeta {
            name: name.to_owned(),
            data_type: udt_name.to_owned(),
            udt_name: udt_name.to_owned(),
            nullable: !pk,
            default_expr: default_expr.map(str::to_owned),
            is_primary_key: pk,
            is_unique: false,
            comment: None,
            foreign_key: None,
        }
    }

    #[test]
    fn mysql_and_sqlite_staging_sql_dialects() {
        let cols = vec![
            col("id", "int", true, Some("auto_increment")),
            col("active", "tinyint(1)", false, None),
            col("note", "varchar", false, None),
            col("badge", "blob", false, None),
        ];

        assert_eq!(escape_mysql_string(r"a\b'c"), r"a\\b''c");
        assert_eq!(
            format_mysql_string_literal(r"a\b'c"),
            "CONCAT('a', CHAR(92 USING utf8mb4), 'b''c')"
        );
        assert!(!format_mysql_string_literal(r"\\a\b").contains('\\'));

        let mut staged = StagedChanges::new();
        let row0 = vec![
            CellValue::Int(1),
            CellValue::Bool(true),
            CellValue::Text("hi".to_owned()),
            CellValue::Bytes(vec![0xca, 0xfe]),
        ];
        staged.stage_cell_edit(0, &row0, 1, StagedCellValue::Literal("false".to_owned()));
        staged.stage_cell_edit(0, &row0, 2, StagedCellValue::Literal(r"it's \ok".to_owned()));
        let draft = staged.stage_insert_row(&cols);
        let _ = draft;

        let mysql_tx = staged
            .build_transaction_sql_for_engine(DbEngine::Mysql, "greendale", "students", &cols)
            .expect("mysql tx");
        assert!(mysql_tx.starts_with("START TRANSACTION;\n"));
        assert!(mysql_tx.contains("UPDATE `greendale`.`students` SET `active` = FALSE, `note` = CONCAT('it''s ', CHAR(92 USING utf8mb4), 'ok') WHERE `id` = 1 AND `active` = TRUE AND CAST(CONVERT(`note` USING utf8mb4) AS BINARY) = CAST(CONVERT('hi' USING utf8mb4) AS BINARY) AND `badge` = X'cafe';"));
        assert!(!mysql_tx.contains('\\'));
        assert!(mysql_tx.contains("INSERT INTO `greendale`.`students` (`active`, `note`, `badge`) VALUES (NULL, NULL, NULL);"));

        let sqlite_tx = staged
            .build_transaction_sql_for_engine(DbEngine::Sqlite, "", "students", &cols)
            .expect("sqlite tx");
        assert!(sqlite_tx.starts_with("BEGIN;\n"));
        assert!(sqlite_tx.contains("UPDATE \"students\" SET \"active\" = 0, \"note\" = 'it''s \\ok' WHERE \"id\" = 1 AND \"active\" = 1 AND \"note\" COLLATE BINARY = 'hi' AND \"badge\" = X'cafe';"));

        let mut default_only = StagedChanges::new();
        default_only.stage_insert_row(&[col("id", "int", true, Some("1"))]);
        assert_eq!(
            default_only
                .build_statements_for_engine(DbEngine::Mysql, "", "t", &[col("id", "int", true, Some("1"))])
                .expect("mysql default insert"),
            vec!["INSERT INTO `t` () VALUES ();"]
        );
        assert_eq!(
            default_only
                .build_statements_for_engine(DbEngine::Sqlite, "", "t", &[col("id", "int", true, Some("1"))])
                .expect("sqlite default insert"),
            vec!["INSERT INTO \"t\" DEFAULT VALUES;"]
        );

        assert_eq!(
            format_cell_literal(DbEngine::Mysql, &cols[3], &CellValue::Bytes(vec![0xde, 0xad])),
            "X'dead'"
        );
        assert_eq!(
            format_cell_literal(DbEngine::Sqlite, &cols[1], &CellValue::Bool(true)),
            "1"
        );
        assert_eq!(
            StagedCellValue::Literal("1".to_owned()).to_preview_cell(&cols[1]),
            CellValue::Bool(true)
        );
    }
}
