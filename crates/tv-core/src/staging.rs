use std::collections::BTreeMap;
use std::fmt;

use crate::engine::postgres::quote_ident;
use crate::engine::{CellValue, ColumnMeta};

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

impl StagedCellValue {
    /// Converts a staged cell value into a [`CellValue`] preview for rendering in the grid and inspector.
    pub fn to_preview_cell(&self, col: &ColumnMeta) -> CellValue {
        match self {
            Self::Null => CellValue::Null,
            Self::DefaultExpr => CellValue::Text("DEFAULT".to_owned()),
            Self::Literal(raw) => match col.udt_name.as_str() {
                "bool" => match raw.trim().to_ascii_lowercase().as_str() {
                    "true" | "t" | "1" | "yes" | "y" | "on" => CellValue::Bool(true),
                    "false" | "f" | "0" | "no" | "n" | "off" => CellValue::Bool(false),
                    _ => CellValue::Text(raw.clone()),
                },
                "int2" | "int4" | "int8" => match raw.trim().parse::<i64>() {
                    Ok(v) => CellValue::Int(v),
                    Err(_) => CellValue::Text(raw.clone()),
                },
                "float4" | "float8" => match raw.trim().parse::<f64>() {
                    Ok(v) if v.is_finite() => CellValue::Float(v),
                    _ => CellValue::Text(raw.clone()),
                },
                "numeric" | "money" => CellValue::Numeric(raw.trim().to_owned()),
                "json" | "jsonb" => CellValue::Json(raw.clone()),
                "date" => CellValue::Date(raw.clone()),
                "time" | "timetz" => CellValue::Time(raw.clone()),
                "timestamptz" | "timestamp" => {
                    CellValue::parse_from_pg_text(&col.udt_name, Some(raw.clone()))
                }
                "uuid" => CellValue::parse_from_pg_text("uuid", Some(raw.clone())),
                "bytea" => {
                    if let Some(hex) = raw.strip_prefix("\\x") {
                        CellValue::parse_from_pg_text("bytea", Some(hex.to_owned()))
                    } else {
                        CellValue::parse_from_pg_text("bytea", Some(raw.clone()))
                    }
                }
                _ if col.udt_name.starts_with('_') => CellValue::Array(raw.clone()),
                _ => CellValue::Text(raw.clone()),
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
    pub edits: BTreeMap<usize, StagedCellValue>,
}

/// Staged unsaved insert row awaiting transactional commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StagedInsertRow {
    pub id: DraftRowId,
    pub values: Vec<StagedCellValue>,
}

/// Validation errors returned when generating SQL from staged table changes.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StagingError {
    #[error("table {schema}.{table} requires a primary key to update or delete existing rows safely")]
    MissingPrimaryKey { schema: String, table: String },
    #[error("no staged changes to commit")]
    EmptyChanges,
}

/// Pending inline cell edits, inserted draft rows, and marked-for-delete rows for one table tab.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StagedChanges {
    pub updates: BTreeMap<usize, StagedRowUpdate>,
    pub deletes: BTreeMap<usize, Vec<CellValue>>,
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
            edits: BTreeMap::new(),
        });
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
        if self.deletes.remove(&row_idx).is_some() {
            false
        } else {
            self.updates.remove(&row_idx);
            self.deletes.insert(row_idx, original_row.to_vec());
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

    /// Generates parameterized-safe SQL statements (single transaction order: updates, inserts, deletes).
    pub fn build_statements(
        &self,
        schema: &str,
        table: &str,
        columns: &[ColumnMeta],
    ) -> Result<Vec<String>, StagingError> {
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

        let fq_table = format!("{}.{}", quote_ident(schema), quote_ident(table));
        let mut stmts = Vec::with_capacity(self.updates.len() + self.inserts.len() + self.deletes.len());

        // 1. Updates
        for (row_idx, upd) in &self.updates {
            if self.deletes.contains_key(row_idx) || upd.edits.is_empty() {
                continue;
            }
            let set_clauses: Vec<String> = upd
                .edits
                .iter()
                .filter_map(|(&col_idx, val)| {
                    let col = columns.get(col_idx)?;
                    Some(format!("{} = {}", quote_ident(&col.name), format_pg_literal(col, val)))
                })
                .collect();
            if set_clauses.is_empty() {
                continue;
            }
            let where_sql = build_pk_where(&pk_indices, columns, &upd.original_row);
            stmts.push(format!("UPDATE {fq_table} SET {} WHERE {where_sql};", set_clauses.join(", ")));
        }

        // 2. Inserts
        for draft in &self.inserts {
            let mut col_names = Vec::new();
            let mut val_exprs = Vec::new();
            for (col_idx, col) in columns.iter().enumerate() {
                let staged_val = draft.values.get(col_idx).unwrap_or(&StagedCellValue::DefaultExpr);
                if matches!(staged_val, StagedCellValue::DefaultExpr) {
                    continue;
                }
                col_names.push(quote_ident(&col.name));
                val_exprs.push(format_pg_literal(col, staged_val));
            }
            if col_names.is_empty() {
                stmts.push(format!("INSERT INTO {fq_table} DEFAULT VALUES;"));
            } else {
                stmts.push(format!(
                    "INSERT INTO {fq_table} ({}) VALUES ({});",
                    col_names.join(", "),
                    val_exprs.join(", ")
                ));
            }
        }

        // 3. Deletes
        for original_row in self.deletes.values() {
            let where_sql = build_pk_where(&pk_indices, columns, original_row);
            stmts.push(format!("DELETE FROM {fq_table} WHERE {where_sql};"));
        }

        if stmts.is_empty() {
            return Err(StagingError::EmptyChanges);
        }
        Ok(stmts)
    }

    /// Formats all staged changes as a readable `BEGIN; ... COMMIT;` SQL script.
    pub fn build_transaction_sql(
        &self,
        schema: &str,
        table: &str,
        columns: &[ColumnMeta],
    ) -> Result<String, StagingError> {
        let stmts = self.build_statements(schema, table, columns)?;
        Ok(format!("BEGIN;\n\n{}\n\nCOMMIT;\n", stmts.join("\n")))
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

        for (row_idx, original_row) in &self.deletes {
            let row_tag = format_row_label(*row_idx, &pk_indices, columns, original_row);
            before.push_str(&format!("-- {schema}.{table} {row_tag} [DELETE]\n"));
            for (col_idx, col) in columns.iter().enumerate() {
                let orig = original_row.get(col_idx).unwrap_or(&CellValue::Null);
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

fn build_pk_where(pk_indices: &[usize], columns: &[ColumnMeta], row: &[CellValue]) -> String {
    let clauses: Vec<String> = pk_indices
        .iter()
        .filter_map(|&idx| {
            let col = columns.get(idx)?;
            let val = row.get(idx).unwrap_or(&CellValue::Null);
            if val.is_null() {
                Some(format!("{} IS NULL", quote_ident(&col.name)))
            } else {
                Some(format!("{} = {}", quote_ident(&col.name), format_pg_cell_literal(col, val)))
            }
        })
        .collect();
    clauses.join(" AND ")
}

/// Escapes single quotes inside SQL string literals by doubling `'`.
pub fn escape_sql_string(raw: &str) -> String {
    raw.replace('\'', "''")
}

/// Formats a staged cell edit into a type-safe PostgreSQL literal or expression.
pub fn format_pg_literal(col: &ColumnMeta, val: &StagedCellValue) -> String {
    match val {
        StagedCellValue::Null => "NULL".to_owned(),
        StagedCellValue::DefaultExpr => "DEFAULT".to_owned(),
        StagedCellValue::Literal(raw) => format_raw_pg_literal(col, raw),
    }
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

/// Builds a `WHERE` filter snippet targeting the referenced foreign-key row, returning `None` for `NULL`.
pub fn build_fk_where_clause(
    target_column: &str,
    value: &CellValue,
    source_col: &ColumnMeta,
) -> Option<String> {
    if value.is_null() {
        return None;
    }
    Some(format!(
        "{} = {}",
        quote_ident(target_column),
        format_pg_cell_literal(source_col, value)
    ))
}
