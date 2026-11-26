use serde_json::{Map, Number, Value};

use crate::engine::postgres::quote_ident;
use crate::engine::{CellValue, ColumnMeta};
use crate::staging::format_pg_cell_literal;

/// Supported data export formats for table pages and SQL result sets.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExportFormat {
    #[default]
    Csv,
    Json,
    SqlInsert,
}

impl ExportFormat {
    pub const ALL: [Self; 3] = [Self::Csv, Self::Json, Self::SqlInsert];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Csv => "CSV",
            Self::Json => "JSON",
            Self::SqlInsert => "SQL INSERT",
        }
    }

    pub const fn extension(self) -> &'static str {
        match self {
            Self::Csv => "csv",
            Self::Json => "json",
            Self::SqlInsert => "sql",
        }
    }

    pub const fn filter_name(self) -> &'static str {
        match self {
            Self::Csv => "CSV",
            Self::Json => "JSON",
            Self::SqlInsert => "SQL",
        }
    }

    pub const fn index(self) -> usize {
        match self {
            Self::Csv => 0,
            Self::Json => 1,
            Self::SqlInsert => 2,
        }
    }

    pub const fn from_index(index: usize) -> Self {
        match index {
            1 => Self::Json,
            2 => Self::SqlInsert,
            _ => Self::Csv,
        }
    }
}

/// Suggests a clean default export filename such as `students.csv` or `query_result.json`.
pub fn default_export_filename(table_name: &str, format: ExportFormat) -> String {
    let clean: String = table_name
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let stem = if clean.is_empty() { "export" } else { clean.as_str() };
    format!("{stem}.{}", format.extension())
}

/// Exports tabular rows into CSV (RFC 4180 escaping) or JSON or PostgreSQL `INSERT` statements.
pub fn export_rows(
    format: ExportFormat,
    schema: Option<&str>,
    table_name: &str,
    columns: &[ColumnMeta],
    rows: &[Vec<CellValue>],
) -> String {
    match format {
        ExportFormat::Csv => export_csv(columns, rows),
        ExportFormat::Json => export_json(columns, rows),
        ExportFormat::SqlInsert => export_sql_inserts(schema, table_name, columns, rows),
    }
}

/// Encodes columns and rows as RFC 4180 CSV (`NULL` rendered as empty cell).
pub fn export_csv(columns: &[ColumnMeta], rows: &[Vec<CellValue>]) -> String {
    let mut out = String::new();
    for (idx, col) in columns.iter().enumerate() {
        if idx > 0 {
            out.push(',');
        }
        push_csv_field(&mut out, &col.name);
    }
    out.push('\n');

    for row in rows {
        for (idx, _) in columns.iter().enumerate() {
            if idx > 0 {
                out.push(',');
            }
            let val = row.get(idx).unwrap_or(&CellValue::Null);
            if !val.is_null() {
                push_csv_field(&mut out, &val.copy_text());
            }
        }
        out.push('\n');
    }
    out
}

fn push_csv_field(out: &mut String, raw: &str) {
    let needs_quotes = raw.contains([',', '"', '\n', '\r']);
    if !needs_quotes {
        out.push_str(raw);
        return;
    }
    out.push('"');
    for ch in raw.chars() {
        if ch == '"' {
            out.push_str("\"\"");
        } else {
            out.push(ch);
        }
    }
    out.push('"');
}

/// Encodes rows as a pretty-printed JSON array, preserving booleans, integers, floats, nulls, and JSONB values.
pub fn export_json(columns: &[ColumnMeta], rows: &[Vec<CellValue>]) -> String {
    let items: Vec<Value> = rows
        .iter()
        .map(|row| {
            let mut obj = Map::new();
            for (idx, col) in columns.iter().enumerate() {
                let val = row.get(idx).unwrap_or(&CellValue::Null);
                let json_val = cell_to_json(val);
                // Preserve duplicate column names cleanly if a SQL query returned duplicate aliases.
                if !obj.contains_key(&col.name) {
                    obj.insert(col.name.clone(), json_val);
                } else {
                    obj.insert(format!("{}_{idx}", col.name), json_val);
                }
            }
            Value::Object(obj)
        })
        .collect();

    let mut rendered = serde_json::to_string_pretty(&Value::Array(items)).unwrap_or_else(|_| "[]".to_owned());
    rendered.push('\n');
    rendered
}

fn cell_to_json(val: &CellValue) -> Value {
    match val {
        CellValue::Null => Value::Null,
        CellValue::Bool(b) => Value::Bool(*b),
        CellValue::Int(i) => Value::Number(Number::from(*i)),
        CellValue::Float(f) => Number::from_f64(*f)
            .map(Value::Number)
            .unwrap_or_else(|| Value::String(f.to_string())),
        CellValue::Json(raw) => {
            serde_json::from_str::<Value>(raw).unwrap_or_else(|_| Value::String(raw.clone()))
        }
        other => Value::String(other.copy_text()),
    }
}

/// Encodes rows as deterministic PostgreSQL `INSERT INTO ... VALUES (...);` statements.
pub fn export_sql_inserts(
    schema: Option<&str>,
    table_name: &str,
    columns: &[ColumnMeta],
    rows: &[Vec<CellValue>],
) -> String {
    if columns.is_empty() || rows.is_empty() {
        return String::new();
    }
    let clean_table = if table_name.trim().is_empty() {
        "exported_table"
    } else {
        table_name.trim()
    };
    let target = match schema.map(str::trim).filter(|s| !s.is_empty()) {
        Some(sc) => format!("{}.{}", quote_ident(sc), quote_ident(clean_table)),
        None => quote_ident(clean_table),
    };
    let col_list: String = columns
        .iter()
        .map(|c| quote_ident(&c.name))
        .collect::<Vec<_>>()
        .join(", ");

    let mut out = String::new();
    for row in rows {
        let vals: String = columns
            .iter()
            .enumerate()
            .map(|(idx, col)| {
                let cell = row.get(idx).unwrap_or(&CellValue::Null);
                format_pg_cell_literal(col, cell)
            })
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!("INSERT INTO {target} ({col_list}) VALUES ({vals});\n"));
    }
    out
}
