use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value};

use crate::connection::DbEngine;
use crate::engine::{CellValue, QueryStatementResult};

/// Errors returned when parsing PostgreSQL `EXPLAIN (FORMAT JSON)` output.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ExplainError {
    #[error("invalid EXPLAIN JSON output: {0}")]
    InvalidJson(String),
    #[error("EXPLAIN output returned an empty plan array")]
    EmptyPlan,
    #[error("EXPLAIN JSON root object does not contain a 'Plan' field")]
    MissingPlanRoot,
}

/// Complete parsed execution plan returned by PostgreSQL `EXPLAIN (FORMAT JSON)`.
#[derive(Clone, Debug, PartialEq)]
pub struct ExplainPlan {
    pub sql: String,
    pub analyzed: bool,
    pub planning_time_ms: Option<f64>,
    pub execution_time_ms: Option<f64>,
    pub root: ExplainNode,
    pub raw_json: String,
}

/// Single execution operator node inside a PostgreSQL plan tree.
#[derive(Clone, Debug, PartialEq)]
pub struct ExplainNode {
    pub node_type: String,
    pub relation_name: Option<String>,
    pub schema: Option<String>,
    pub alias: Option<String>,
    pub index_name: Option<String>,
    pub join_type: Option<String>,
    pub startup_cost: f64,
    pub total_cost: f64,
    pub plan_rows: u64,
    pub plan_width: u64,
    pub actual_startup_time_ms: Option<f64>,
    pub actual_total_time_ms: Option<f64>,
    pub actual_rows: Option<u64>,
    pub actual_loops: Option<f64>,
    pub filter: Option<String>,
    pub index_cond: Option<String>,
    pub hash_cond: Option<String>,
    pub sort_key: Vec<String>,
    pub shared_hit_blocks: Option<u64>,
    pub shared_read_blocks: Option<u64>,
    pub rows_removed_by_filter: Option<u64>,
    pub children: Vec<ExplainNode>,
}

/// Builds the SQL query string sent to PostgreSQL for JSON plan inspection.
pub fn build_explain_sql(statement: &str, analyze: bool) -> String {
    build_explain_sql_for_engine(DbEngine::Postgres, statement, analyze)
}

/// Builds the dialect-appropriate `EXPLAIN` query string for `engine`.
pub fn build_explain_sql_for_engine(engine: DbEngine, statement: &str, analyze: bool) -> String {
    let trimmed = statement.trim().trim_end_matches(';').trim();
    match engine {
        DbEngine::Postgres => {
            if analyze {
                format!("EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) {trimmed}")
            } else {
                format!("EXPLAIN (FORMAT JSON) {trimmed}")
            }
        }
        DbEngine::Mysql => {
            if analyze {
                format!("EXPLAIN ANALYZE {trimmed}")
            } else {
                format!("EXPLAIN FORMAT=JSON {trimmed}")
            }
        }
        DbEngine::Sqlite => format!("EXPLAIN QUERY PLAN {trimmed}"),
    }
}

impl ExplainPlan {
    /// Parses PostgreSQL `EXPLAIN (FORMAT JSON)` text into a structured plan tree.
    pub fn parse_json(sql: String, analyzed: bool, raw_json: &str) -> Result<Self, ExplainError> {
        let parsed: Value =
            serde_json::from_str(raw_json.trim()).map_err(|e| ExplainError::InvalidJson(e.to_string()))?;
        let pretty_raw = serde_json::to_string_pretty(&parsed).unwrap_or_else(|_| raw_json.trim().to_owned());

        let first = match parsed {
            Value::Array(arr) => arr.into_iter().next().ok_or(ExplainError::EmptyPlan)?,
            obj @ Value::Object(_) => obj,
            _ => return Err(ExplainError::MissingPlanRoot),
        };

        let planning_time_ms = first.get("Planning Time").and_then(Value::as_f64);
        let execution_time_ms = first.get("Execution Time").and_then(Value::as_f64);
        let plan_val = first.get("Plan").ok_or(ExplainError::MissingPlanRoot)?;
        let root = parse_node(plan_val)?;
        let inferred_analyzed = analyzed
            || execution_time_ms.is_some()
            || root.actual_total_time_ms.is_some();

        Ok(Self {
            sql,
            analyzed: inferred_analyzed,
            planning_time_ms,
            execution_time_ms,
            root,
            raw_json: pretty_raw,
        })
    }

    /// Parses MySQL `EXPLAIN FORMAT=JSON` text into a structured [`ExplainPlan`] tree.
    pub fn parse_mysql_json(sql: String, raw_json: &str) -> Result<Self, ExplainError> {
        let parsed: Value =
            serde_json::from_str(raw_json.trim()).map_err(|e| ExplainError::InvalidJson(e.to_string()))?;
        let pretty_raw = serde_json::to_string_pretty(&parsed).unwrap_or_else(|_| raw_json.trim().to_owned());

        let root_obj = parsed.as_object().ok_or(ExplainError::MissingPlanRoot)?;
        let root = if let Some(qb) = root_obj.get("query_block").and_then(Value::as_object) {
            parse_mysql_query_block(qb)?
        } else if root_obj.contains_key("table") || root_obj.contains_key("nested_loop") {
            parse_mysql_query_block(root_obj)?
        } else {
            return Err(ExplainError::MissingPlanRoot);
        };

        Ok(Self {
            sql,
            analyzed: false,
            planning_time_ms: None,
            execution_time_ms: None,
            root,
            raw_json: pretty_raw,
        })
    }

    /// Parses MySQL `EXPLAIN ANALYZE` indented tree text into a structured [`ExplainPlan`].
    pub fn parse_mysql_analyze_tree(sql: String, raw_tree: &str) -> Result<Self, ExplainError> {
        let mut stack: Vec<(usize, ExplainNode)> = Vec::new();
        let mut top_nodes: Vec<ExplainNode> = Vec::new();

        for line in raw_tree.lines() {
            let trimmed_end = line.trim_end();
            if trimmed_end.trim().is_empty() {
                continue;
            }
            let Some(arrow_pos) = trimmed_end.find("-> ") else {
                continue;
            };
            let prefix = &trimmed_end[..arrow_pos];
            if !prefix.chars().all(char::is_whitespace) {
                continue;
            }
            let indent = prefix.len();
            let body = trimmed_end[arrow_pos + 3..].trim();
            let node = parse_mysql_analyze_line(body);

            while let Some((top_indent, _)) = stack.last() {
                if indent > *top_indent {
                    break;
                }
                if let Some((_, finished)) = stack.pop() {
                    if let Some((_, parent)) = stack.last_mut() {
                        parent.children.push(finished);
                    } else {
                        top_nodes.push(finished);
                    }
                }
            }
            stack.push((indent, node));
        }

        while let Some((_, finished)) = stack.pop() {
            if let Some((_, parent)) = stack.last_mut() {
                parent.children.push(finished);
            } else {
                top_nodes.push(finished);
            }
        }

        if top_nodes.is_empty() {
            return Err(ExplainError::MissingPlanRoot);
        }

        let root = if top_nodes.len() == 1 {
            top_nodes.into_iter().next().unwrap_or_else(|| default_explain_node("Query Plan".to_owned()))
        } else {
            let total_cost = top_nodes.iter().map(|n| n.total_cost).sum();
            let mut wrapper = default_explain_node("Query Plan".to_owned());
            wrapper.total_cost = total_cost;
            wrapper.children = top_nodes;
            wrapper
        };

        let execution_time_ms = root.inclusive_time_ms();
        let json_val = explain_node_to_json(&root);
        let raw_json = serde_json::to_string_pretty(&json_val).unwrap_or_else(|_| raw_tree.trim().to_owned());

        Ok(Self {
            sql,
            analyzed: true,
            planning_time_ms: None,
            execution_time_ms,
            root,
            raw_json,
        })
    }

    /// Parses SQLite `EXPLAIN QUERY PLAN` `(id, parent, detail)` rows into a structured [`ExplainPlan`].
    pub fn parse_sqlite_query_plan(sql: String, rows: &[(i64, i64, String)]) -> Result<Self, ExplainError> {
        if rows.is_empty() {
            return Err(ExplainError::EmptyPlan);
        }

        let id_set: HashSet<i64> = rows.iter().map(|(id, _, _)| *id).collect();
        let mut nodes_by_id: HashMap<i64, ExplainNode> = HashMap::with_capacity(rows.len());
        let mut children_by_parent: HashMap<i64, Vec<i64>> = HashMap::new();
        let mut root_ids: Vec<i64> = Vec::new();

        for (id, parent, detail) in rows {
            nodes_by_id.entry(*id).or_insert_with(|| parse_sqlite_detail_node(detail));
            if *parent == 0 || !id_set.contains(parent) {
                root_ids.push(*id);
            } else {
                children_by_parent.entry(*parent).or_default().push(*id);
            }
        }

        if root_ids.is_empty()
            && let Some((first_id, _, _)) = rows.first()
        {
            root_ids.push(*first_id);
        }

        let mut visited = HashSet::new();
        let mut built_roots = Vec::with_capacity(root_ids.len());
        for rid in root_ids {
            if let Some(node) = build_sqlite_subtree(rid, &nodes_by_id, &children_by_parent, &mut visited) {
                built_roots.push(node);
            }
        }

        if built_roots.is_empty() {
            return Err(ExplainError::EmptyPlan);
        }

        let root = if built_roots.len() == 1 {
            built_roots
                .into_iter()
                .next()
                .unwrap_or_else(|| default_explain_node("QUERY PLAN".to_owned()))
        } else {
            let mut wrapper = default_explain_node("QUERY PLAN".to_owned());
            wrapper.children = built_roots;
            wrapper
        };

        let raw_json = serde_json::to_string_pretty(&explain_node_to_json(&root))
            .unwrap_or_else(|_| "{}".to_owned());

        Ok(Self {
            sql,
            analyzed: false,
            planning_time_ms: None,
            execution_time_ms: None,
            root,
            raw_json,
        })
    }

    /// Detects and parses `EXPLAIN` output returned directly inside a SQL editor result set
    /// for PostgreSQL JSON, MySQL JSON/tree, and SQLite `EXPLAIN QUERY PLAN`.
    pub fn try_extract_from_result(res: &QueryStatementResult) -> Option<Self> {
        if res.rows.is_empty() {
            return None;
        }
        if res.columns.len() == 1 {
            let col_name = &res.columns[0].name;
            if col_name.eq_ignore_ascii_case("QUERY PLAN") {
                let combined = combine_single_column_rows(&res.rows);
                let trimmed = combined.trim();
                if !trimmed.starts_with('[') && !trimmed.starts_with('{') {
                    return None;
                }
                return Self::parse_json(res.sql.clone(), false, trimmed).ok();
            }
            if col_name.eq_ignore_ascii_case("EXPLAIN") {
                let combined = combine_single_column_rows(&res.rows);
                let trimmed = combined.trim();
                if trimmed.starts_with('{') {
                    return Self::parse_mysql_json(res.sql.clone(), trimmed).ok();
                }
                if trimmed.contains("-> ") {
                    return Self::parse_mysql_analyze_tree(res.sql.clone(), trimmed).ok();
                }
            }
            return None;
        }

        if res.columns.len() == 4
            && res.columns[0].name.eq_ignore_ascii_case("id")
            && res.columns[1].name.eq_ignore_ascii_case("parent")
            && res.columns[2].name.eq_ignore_ascii_case("notused")
            && res.columns[3].name.eq_ignore_ascii_case("detail")
        {
            let mut plan_rows = Vec::with_capacity(res.rows.len());
            for row in &res.rows {
                let id = cell_to_i64(row.first()?)?;
                let parent = cell_to_i64(row.get(1)?)?;
                let detail = row.get(3)?.copy_text();
                plan_rows.push((id, parent, detail));
            }
            return Self::parse_sqlite_query_plan(res.sql.clone(), &plan_rows).ok();
        }

        None
    }

    /// Total cost across the entire plan tree.
    pub fn total_cost(&self) -> f64 {
        self.root.total_cost
    }

    /// Best overall runtime for computing operator percentages when `EXPLAIN ANALYZE` ran.
    pub fn total_runtime_ms(&self) -> Option<f64> {
        self.execution_time_ms.or_else(|| self.root.inclusive_time_ms())
    }

    /// Total number of operator nodes in the plan tree.
    pub fn node_count(&self) -> usize {
        self.root.node_count()
    }

    /// Total shared buffer hits across the plan tree (from `EXPLAIN (ANALYZE, BUFFERS)`).
    pub fn total_shared_hits(&self) -> u64 {
        self.root.shared_hit_blocks.unwrap_or(0)
    }

    /// Total shared disk block reads across the plan tree (from `EXPLAIN (ANALYZE, BUFFERS)`).
    pub fn total_shared_reads(&self) -> u64 {
        self.root.shared_read_blocks.unwrap_or(0)
    }
}

impl ExplainNode {
    /// Total time spent in this node and all of its children (`actual_total_time * actual_loops`).
    pub fn inclusive_time_ms(&self) -> Option<f64> {
        let total = self.actual_total_time_ms?;
        let loops = self.actual_loops.unwrap_or(1.0);
        Some(total * loops)
    }

    /// Self-time spent inside this operator excluding direct child subtrees.
    pub fn exclusive_time_ms(&self) -> Option<f64> {
        let inc = self.inclusive_time_ms()?;
        let child_sum: f64 = self
            .children
            .iter()
            .filter_map(Self::inclusive_time_ms)
            .sum();
        Some((inc - child_sum).max(0.0))
    }

    /// Self-cost excluding direct child subtrees' startup/total cost.
    pub fn exclusive_cost(&self) -> f64 {
        let child_sum: f64 = self.children.iter().map(|c| c.total_cost).sum();
        (self.total_cost - child_sum).max(0.0)
    }

    /// Fraction in `0.0..=1.0` representing this operator's share of total runtime or total cost.
    pub fn share_fraction(&self, root_time: Option<f64>, root_cost: f64) -> f32 {
        if let (Some(total_ms), Some(self_ms)) = (root_time, self.exclusive_time_ms())
            && total_ms > 0.0
        {
            return ((self_ms / total_ms) as f32).clamp(0.0, 1.0);
        }
        if root_cost > 0.0 {
            ((self.exclusive_cost() / root_cost) as f32).clamp(0.0, 1.0)
        } else {
            0.0
        }
    }

    /// Returns divergence factor if actual rows diverge from planner estimate by at least 10x (or 0.1x).
    pub fn row_estimate_ratio(&self) -> Option<f64> {
        let actual = self.actual_rows?;
        let est = self.plan_rows.max(1) as f64;
        let ratio = actual as f64 / est;
        if ratio >= 10.0 || (actual > 0 && ratio <= 0.1) || (actual == 0 && self.plan_rows >= 50) {
            Some(ratio)
        } else {
            None
        }
    }

    /// Human-friendly target relation/index summary shown beside the node type badge.
    pub fn relation_display(&self) -> Option<String> {
        match (&self.schema, &self.relation_name, &self.index_name) {
            (Some(s), Some(r), Some(idx)) => Some(format!("{s}.{r} via {idx}")),
            (None, Some(r), Some(idx)) => Some(format!("{r} via {idx}")),
            (Some(s), Some(r), None) => Some(format!("{s}.{r}")),
            (None, Some(r), None) => Some(r.clone()),
            (_, None, Some(idx)) => Some(idx.clone()),
            _ => None,
        }
    }

    pub fn node_count(&self) -> usize {
        1 + self.children.iter().map(Self::node_count).sum::<usize>()
    }
}

fn parse_node(val: &Value) -> Result<ExplainNode, ExplainError> {
    let obj = val.as_object().ok_or(ExplainError::MissingPlanRoot)?;
    let node_type = obj
        .get("Node Type")
        .and_then(Value::as_str)
        .ok_or_else(|| ExplainError::InvalidJson("plan node missing 'Node Type'".to_owned()))?
        .to_owned();

    let children = match obj.get("Plans") {
        Some(Value::Array(items)) => items.iter().map(parse_node).collect::<Result<Vec<_>, _>>()?,
        _ => Vec::new(),
    };

    let sort_key = match obj.get("Sort Key") {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    };

    Ok(ExplainNode {
        node_type,
        relation_name: str_field(obj, "Relation Name"),
        schema: str_field(obj, "Schema"),
        alias: str_field(obj, "Alias"),
        index_name: str_field(obj, "Index Name"),
        join_type: str_field(obj, "Join Type"),
        startup_cost: obj.get("Startup Cost").and_then(Value::as_f64).unwrap_or(0.0),
        total_cost: obj.get("Total Cost").and_then(Value::as_f64).unwrap_or(0.0),
        plan_rows: obj.get("Plan Rows").and_then(Value::as_u64).unwrap_or(0),
        plan_width: obj.get("Plan Width").and_then(Value::as_u64).unwrap_or(0),
        actual_startup_time_ms: obj.get("Actual Startup Time").and_then(Value::as_f64),
        actual_total_time_ms: obj.get("Actual Total Time").and_then(Value::as_f64),
        actual_rows: obj.get("Actual Rows").and_then(Value::as_u64),
        actual_loops: obj.get("Actual Loops").and_then(Value::as_f64),
        filter: str_field(obj, "Filter"),
        index_cond: str_field(obj, "Index Cond"),
        hash_cond: str_field(obj, "Hash Cond"),
        sort_key,
        shared_hit_blocks: obj.get("Shared Hit Blocks").and_then(Value::as_u64),
        shared_read_blocks: obj.get("Shared Read Blocks").and_then(Value::as_u64),
        rows_removed_by_filter: obj.get("Rows Removed by Filter").and_then(Value::as_u64),
        children,
    })
}

fn str_field(obj: &Map<String, Value>, key: &str) -> Option<String> {
    obj.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn default_explain_node(node_type: String) -> ExplainNode {
    ExplainNode {
        node_type,
        relation_name: None,
        schema: None,
        alias: None,
        index_name: None,
        join_type: None,
        startup_cost: 0.0,
        total_cost: 0.0,
        plan_rows: 0,
        plan_width: 0,
        actual_startup_time_ms: None,
        actual_total_time_ms: None,
        actual_rows: None,
        actual_loops: None,
        filter: None,
        index_cond: None,
        hash_cond: None,
        sort_key: Vec::new(),
        shared_hit_blocks: None,
        shared_read_blocks: None,
        rows_removed_by_filter: None,
        children: Vec::new(),
    }
}

fn combine_single_column_rows(rows: &[Vec<CellValue>]) -> String {
    let mut combined = String::new();
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            combined.push('\n');
        }
        if let Some(cell) = row.first()
            && !cell.is_null()
        {
            combined.push_str(&cell.copy_text());
        }
    }
    combined
}

fn cell_to_i64(cell: &CellValue) -> Option<i64> {
    match cell {
        CellValue::Int(v) => Some(*v),
        CellValue::Numeric(s) | CellValue::Text(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

fn json_f64(val: Option<&Value>) -> Option<f64> {
    match val? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn json_u64(val: &Value) -> Option<u64> {
    match val {
        Value::Number(n) => n.as_u64().or_else(|| n.as_f64().map(|f| f.max(0.0) as u64)),
        Value::String(s) => s.trim().parse::<u64>().ok(),
        _ => None,
    }
}

fn parse_mysql_query_block(obj: &Map<String, Value>) -> Result<ExplainNode, ExplainError> {
    let query_cost = obj.get("cost_info").and_then(|c| json_f64(c.get("query_cost")));
    let mut main_children = parse_mysql_inner_ops(obj)?;

    for sub_key in ["attached_subqueries", "optimized_away_subqueries", "subqueries"] {
        if let Some(Value::Array(subs)) = obj.get(sub_key) {
            for item in subs {
                if let Some(sub_qb) = item.get("query_block").and_then(Value::as_object) {
                    main_children.push(parse_mysql_query_block(sub_qb)?);
                }
            }
        }
    }

    if main_children.is_empty() {
        if let Some(msg) = str_field(obj, "message") {
            let mut node = default_explain_node(msg);
            node.total_cost = query_cost.unwrap_or(0.0);
            return Ok(node);
        }
        return Err(ExplainError::MissingPlanRoot);
    }

    if main_children.len() == 1 {
        let mut only = main_children
            .into_iter()
            .next()
            .unwrap_or_else(|| default_explain_node("Query Block".to_owned()));
        if let Some(cost) = query_cost {
            only.total_cost = only.total_cost.max(cost);
        }
        return Ok(only);
    }

    let total_cost = query_cost.unwrap_or_else(|| main_children.iter().map(|c| c.total_cost).sum());
    let mut node = default_explain_node("Query Block".to_owned());
    node.total_cost = total_cost;
    node.children = main_children;
    Ok(node)
}

fn parse_mysql_inner_ops(obj: &Map<String, Value>) -> Result<Vec<ExplainNode>, ExplainError> {
    let mut out = Vec::new();
    for (key, label) in [
        ("ordering_operation", "Sort / Ordering"),
        ("grouping_operation", "Group / Aggregate"),
        ("duplicates_removal", "Duplicates Removal"),
        ("buffer_result", "Buffer Result"),
    ] {
        if let Some(op_obj) = obj.get(key).and_then(Value::as_object) {
            out.push(parse_mysql_wrapper_op(label, op_obj)?);
        }
    }

    if let Some(Value::Array(items)) = obj.get("nested_loop") {
        out.push(parse_mysql_nested_loop(items)?);
    }
    if let Some(tbl) = obj.get("table").and_then(Value::as_object) {
        out.push(parse_mysql_table(tbl)?);
    }
    if let Some(mat) = obj.get("materialized_from_subquery").and_then(Value::as_object) {
        let mut node = default_explain_node("Materialize".to_owned());
        if let Some(qb) = mat.get("query_block").and_then(Value::as_object) {
            node.children.push(parse_mysql_query_block(qb)?);
        }
        out.push(node);
    }
    if let Some(union_obj) = obj.get("union_result").and_then(Value::as_object) {
        let mut node = default_explain_node("Union".to_owned());
        if let Some(Value::Array(specs)) = union_obj.get("query_specifications") {
            for spec in specs {
                if let Some(qb) = spec.get("query_block").and_then(Value::as_object) {
                    node.children.push(parse_mysql_query_block(qb)?);
                }
            }
        }
        out.push(node);
    }
    Ok(out)
}

fn parse_mysql_wrapper_op(label: &str, obj: &Map<String, Value>) -> Result<ExplainNode, ExplainError> {
    let mut flags = Vec::new();
    if obj.get("using_filesort").and_then(Value::as_bool) == Some(true) {
        flags.push("filesort");
    }
    if obj.get("using_temporary_table").and_then(Value::as_bool) == Some(true) {
        flags.push("temp table");
    }
    let node_type = if flags.is_empty() {
        label.to_owned()
    } else {
        format!("{label} ({})", flags.join(", "))
    };
    let mut node = default_explain_node(node_type);
    node.children = parse_mysql_inner_ops(obj)?;
    node.total_cost = obj
        .get("cost_info")
        .and_then(|c| json_f64(c.get("sort_cost")).or_else(|| json_f64(c.get("query_cost"))))
        .unwrap_or_else(|| node.children.iter().map(|c| c.total_cost).sum());
    Ok(node)
}

fn parse_mysql_nested_loop(items: &[Value]) -> Result<ExplainNode, ExplainError> {
    let mut children = Vec::with_capacity(items.len());
    for item in items {
        if let Some(obj) = item.as_object() {
            if let Some(tbl) = obj.get("table").and_then(Value::as_object) {
                children.push(parse_mysql_table(tbl)?);
            } else {
                children.extend(parse_mysql_inner_ops(obj)?);
            }
        }
    }
    let total_cost = children.last().map_or(0.0, |c| c.total_cost);
    let plan_rows = children.last().map_or(0, |c| c.plan_rows);
    let mut node = default_explain_node("Nested Loop".to_owned());
    node.total_cost = total_cost;
    node.plan_rows = plan_rows;
    node.children = children;
    Ok(node)
}

fn parse_mysql_table(obj: &Map<String, Value>) -> Result<ExplainNode, ExplainError> {
    let access_type = str_field(obj, "access_type");
    let node_type = match access_type.as_deref() {
        Some("ALL") => "Table Scan (ALL)".to_owned(),
        Some("index") => "Index Scan (index)".to_owned(),
        Some("range") => "Index Range Scan".to_owned(),
        Some("ref" | "ref_or_null") => "Index Lookup (ref)".to_owned(),
        Some("eq_ref") => "Unique Index Lookup (eq_ref)".to_owned(),
        Some("const" | "system") => format!("Const Lookup ({})", access_type.as_deref().unwrap_or("const")),
        Some("index_merge") => "Index Merge".to_owned(),
        Some(other) => format!("Table Access ({other})"),
        None => str_field(obj, "message").unwrap_or_else(|| "Table Access".to_owned()),
    };

    let mut children = Vec::new();
    if let Some(mat) = obj.get("materialized_from_subquery").and_then(Value::as_object)
        && let Some(qb) = mat.get("query_block").and_then(Value::as_object)
    {
        children.push(parse_mysql_query_block(qb)?);
    }
    if let Some(Value::Array(subs)) = obj.get("attached_subqueries") {
        for sub in subs {
            if let Some(qb) = sub.get("query_block").and_then(Value::as_object) {
                children.push(parse_mysql_query_block(qb)?);
            }
        }
    }

    let cost_info = obj.get("cost_info");
    let total_cost = cost_info
        .and_then(|c| json_f64(c.get("prefix_cost")).or_else(|| json_f64(c.get("read_cost"))))
        .unwrap_or(0.0);
    let plan_rows = obj
        .get("rows_examined_per_scan")
        .or_else(|| obj.get("rows_produced_per_join"))
        .and_then(json_u64)
        .unwrap_or(0);
    let index_cond = str_field(obj, "index_condition").or_else(|| {
        obj.get("ref").and_then(Value::as_array).map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
    });

    let mut node = default_explain_node(node_type);
    node.relation_name = str_field(obj, "table_name");
    node.index_name = str_field(obj, "key");
    node.total_cost = total_cost;
    node.plan_rows = plan_rows;
    node.filter = str_field(obj, "attached_condition");
    node.index_cond = index_cond.filter(|s| !s.is_empty());
    node.children = children;
    Ok(node)
}

fn parse_mysql_analyze_line(line: &str) -> ExplainNode {
    let mut rest = line.trim();
    let mut actual_startup = None;
    let mut actual_total = None;
    let mut actual_rows = None;
    let mut actual_loops = None;

    if let Some(stripped) = rest.strip_suffix("(never executed)") {
        actual_rows = Some(0);
        actual_loops = Some(0.0);
        rest = stripped.trim_end();
    } else if let Some(idx) = rest.rfind("(actual time=")
        && rest.ends_with(')')
    {
        let inner = &rest[idx + 1..rest.len() - 1];
        for part in inner.split_whitespace() {
            if let Some(times) = part.strip_prefix("time=") {
                if let Some((start_s, end_s)) = times.split_once("..") {
                    actual_startup = start_s.parse::<f64>().ok();
                    actual_total = end_s.parse::<f64>().ok();
                } else {
                    actual_total = times.parse::<f64>().ok();
                }
            } else if let Some(r) = part.strip_prefix("rows=") {
                actual_rows = r.parse::<f64>().ok().map(|v| v.max(0.0).round() as u64);
            } else if let Some(l) = part.strip_prefix("loops=") {
                actual_loops = l.parse::<f64>().ok();
            }
        }
        rest = rest[..idx].trim_end();
    }

    let mut total_cost = 0.0;
    let mut plan_rows = 0u64;
    if let Some(idx) = rest.rfind("(cost=")
        && rest.ends_with(')')
    {
        let inner = &rest[idx + 1..rest.len() - 1];
        for part in inner.split_whitespace() {
            if let Some(c) = part.strip_prefix("cost=") {
                let cost_str = c.split("..").last().unwrap_or(c);
                total_cost = cost_str.parse::<f64>().unwrap_or(0.0);
            } else if let Some(r) = part.strip_prefix("rows=") {
                plan_rows = r.parse::<f64>().ok().map_or(0, |v| v.max(0.0).round() as u64);
            }
        }
        rest = rest[..idx].trim_end();
    }

    let (node_type, relation_name, index_name, filter, index_cond) = parse_mysql_analyze_desc(rest);
    let mut node = default_explain_node(node_type);
    node.relation_name = relation_name;
    node.index_name = index_name;
    node.total_cost = total_cost;
    node.plan_rows = plan_rows;
    node.actual_startup_time_ms = actual_startup;
    node.actual_total_time_ms = actual_total;
    node.actual_rows = actual_rows;
    node.actual_loops = actual_loops;
    node.filter = filter;
    node.index_cond = index_cond;
    node
}

fn parse_mysql_analyze_desc(
    desc: &str,
) -> (String, Option<String>, Option<String>, Option<String>, Option<String>) {
    let trimmed = desc.trim();
    if let Some(cond) = trimmed.strip_prefix("Filter:") {
        return (
            "Filter".to_owned(),
            None,
            None,
            Some(cond.trim().to_owned()),
            None,
        );
    }
    if let Some(idx) = trimmed.find(" on ") {
        let node_type = trimmed[..idx].trim().to_owned();
        let after_on = trimmed[idx + 4..].trim();
        let rel = after_on.split_whitespace().next().map(str::to_owned);
        let mut index_name = None;
        let mut index_cond = None;
        if let Some(using_idx) = after_on.find(" using ") {
            let after_using = after_on[using_idx + 7..].trim();
            let mut parts = after_using.splitn(2, char::is_whitespace);
            index_name = parts.next().map(str::to_owned);
            if let Some(cond_rest) = parts.next() {
                let cond_trim = cond_rest.trim();
                if !cond_trim.is_empty() {
                    index_cond = Some(cond_trim.to_owned());
                }
            }
        }
        return (node_type, rel, index_name, None, index_cond);
    }
    if let Some((head, tail)) = trimmed.split_once(':') {
        return (
            head.trim().to_owned(),
            None,
            None,
            Some(tail.trim().to_owned()),
            None,
        );
    }
    (trimmed.to_owned(), None, None, None, None)
}

fn parse_sqlite_detail_node(detail: &str) -> ExplainNode {
    let trimmed = detail.trim();
    let upper = trimmed.to_ascii_uppercase();

    let (node_type, relation_name, index_name, filter, index_cond) = if upper.starts_with("SCAN ") {
        let (rel, idx, cond) = parse_sqlite_scan_or_search(&trimmed[5..]);
        ("SCAN".to_owned(), rel, idx, None, cond)
    } else if upper.starts_with("SEARCH ") {
        let (rel, idx, cond) = parse_sqlite_scan_or_search(&trimmed[7..]);
        ("SEARCH".to_owned(), rel, idx, None, cond)
    } else if upper.starts_with("USE TEMP B-TREE") {
        let rest = trimmed["USE TEMP B-TREE".len()..].trim();
        (
            "USE TEMP B-TREE".to_owned(),
            None,
            None,
            (!rest.is_empty()).then(|| rest.to_owned()),
            None,
        )
    } else if upper.starts_with("COMPOUND") {
        ("COMPOUND".to_owned(), None, None, Some(trimmed.to_owned()), None)
    } else if upper.starts_with("CO-ROUTINE") {
        let rest = trimmed["CO-ROUTINE".len()..].trim();
        (
            "CO-ROUTINE".to_owned(),
            (!rest.is_empty()).then(|| rest.to_owned()),
            None,
            None,
            None,
        )
    } else if upper.starts_with("MATERIALIZE") {
        let rest = trimmed["MATERIALIZE".len()..].trim();
        (
            "MATERIALIZE".to_owned(),
            (!rest.is_empty()).then(|| rest.to_owned()),
            None,
            None,
            None,
        )
    } else if upper.starts_with("MULTI-INDEX OR") {
        ("MULTI-INDEX OR".to_owned(), None, None, None, None)
    } else if upper.starts_with("MERGE") {
        ("MERGE".to_owned(), None, None, Some(trimmed.to_owned()), None)
    } else {
        let head = trimmed.split_whitespace().next().unwrap_or(trimmed).to_owned();
        let rest = trimmed[head.len()..].trim();
        (
            head,
            None,
            None,
            (!rest.is_empty()).then(|| rest.to_owned()),
            None,
        )
    };

    let mut node = default_explain_node(node_type);
    node.relation_name = relation_name;
    node.index_name = index_name;
    node.filter = filter;
    node.index_cond = index_cond;
    node
}

fn parse_sqlite_scan_or_search(rest: &str) -> (Option<String>, Option<String>, Option<String>) {
    let mut s = rest.trim();
    if s.len() >= 6 && s[..6].eq_ignore_ascii_case("TABLE ") {
        s = s[6..].trim_start();
    }

    let upper = s.to_ascii_uppercase();
    let mut index_name = None;
    let mut index_cond = None;

    let before_using = if let Some(using_pos) = upper.find(" USING ") {
        let after_using = s[using_pos + 7..].trim();
        let after_upper = after_using.to_ascii_uppercase();
        let idx_part = if after_upper.starts_with("AUTOMATIC COVERING INDEX ") {
            after_using["AUTOMATIC COVERING INDEX ".len()..].trim()
        } else if after_upper.starts_with("COVERING INDEX ") {
            after_using["COVERING INDEX ".len()..].trim()
        } else if after_upper.starts_with("INDEX ") {
            after_using["INDEX ".len()..].trim()
        } else if after_upper.starts_with("INTEGER PRIMARY KEY") {
            index_name = Some("PRIMARY KEY".to_owned());
            if let Some(paren_pos) = after_using.find('(') {
                index_cond = Some(after_using[paren_pos..].trim().to_owned());
            }
            ""
        } else {
            after_using
        };

        if !idx_part.is_empty() {
            if let Some(paren_pos) = idx_part.find('(') {
                let name = idx_part[..paren_pos].trim();
                if !name.is_empty() {
                    index_name = Some(name.to_owned());
                }
                index_cond = Some(idx_part[paren_pos..].trim().to_owned());
            } else {
                index_name = idx_part.split_whitespace().next().map(str::to_owned);
            }
        }
        s[..using_pos].trim()
    } else if let Some(paren_pos) = s.find('(') {
        index_cond = Some(s[paren_pos..].trim().to_owned());
        s[..paren_pos].trim()
    } else {
        s
    };

    let relation_name = if before_using.is_empty() {
        None
    } else {
        Some(before_using.to_owned())
    };
    (relation_name, index_name, index_cond)
}

fn build_sqlite_subtree(
    id: i64,
    nodes_by_id: &HashMap<i64, ExplainNode>,
    children_by_parent: &HashMap<i64, Vec<i64>>,
    visited: &mut HashSet<i64>,
) -> Option<ExplainNode> {
    if !visited.insert(id) {
        return None;
    }
    let mut node = nodes_by_id.get(&id)?.clone();
    if let Some(child_ids) = children_by_parent.get(&id) {
        for &cid in child_ids {
            if let Some(child) = build_sqlite_subtree(cid, nodes_by_id, children_by_parent, visited) {
                node.children.push(child);
            }
        }
    }
    Some(node)
}

fn explain_node_to_json(node: &ExplainNode) -> Value {
    let mut obj = Map::new();
    obj.insert("node_type".to_owned(), Value::String(node.node_type.clone()));
    if let Some(rel) = &node.relation_name {
        obj.insert("relation_name".to_owned(), Value::String(rel.clone()));
    }
    if let Some(idx) = &node.index_name {
        obj.insert("index_name".to_owned(), Value::String(idx.clone()));
    }
    if node.total_cost > 0.0 {
        obj.insert("total_cost".to_owned(), serde_json::json!(node.total_cost));
    }
    if node.plan_rows > 0 {
        obj.insert("plan_rows".to_owned(), serde_json::json!(node.plan_rows));
    }
    if let Some(t) = node.actual_total_time_ms {
        obj.insert("actual_total_time_ms".to_owned(), serde_json::json!(t));
    }
    if let Some(r) = node.actual_rows {
        obj.insert("actual_rows".to_owned(), serde_json::json!(r));
    }
    if let Some(l) = node.actual_loops {
        obj.insert("actual_loops".to_owned(), serde_json::json!(l));
    }
    if let Some(f) = &node.filter {
        obj.insert("filter".to_owned(), Value::String(f.clone()));
    }
    if let Some(c) = &node.index_cond {
        obj.insert("index_cond".to_owned(), Value::String(c.clone()));
    }
    if !node.children.is_empty() {
        obj.insert(
            "children".to_owned(),
            Value::Array(node.children.iter().map(explain_node_to_json).collect()),
        );
    }
    Value::Object(obj)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::engine::ColumnMeta;

    #[test]
    fn parses_mysql_json_analyze_and_sqlite_query_plan() {
        assert_eq!(
            build_explain_sql_for_engine(DbEngine::Mysql, "SELECT 1;", false),
            "EXPLAIN FORMAT=JSON SELECT 1"
        );
        assert_eq!(
            build_explain_sql_for_engine(DbEngine::Mysql, "SELECT 1;", true),
            "EXPLAIN ANALYZE SELECT 1"
        );
        assert_eq!(
            build_explain_sql_for_engine(DbEngine::Sqlite, "SELECT 1;", true),
            "EXPLAIN QUERY PLAN SELECT 1"
        );

        let mysql_json = r#"{
          "query_block": {
            "select_id": 1,
            "cost_info": { "query_cost": "3.25" },
            "nested_loop": [
              {
                "table": {
                  "table_name": "students",
                  "access_type": "ref",
                  "key": "idx_group",
                  "rows_examined_per_scan": 12,
                  "cost_info": { "prefix_cost": "1.75" },
                  "attached_condition": "(students.gpa >= 3.0)"
                }
              }
            ]
          }
        }"#;
        let plan = ExplainPlan::parse_mysql_json("SELECT * FROM students".to_owned(), mysql_json)
            .expect("mysql json");
        assert!(!plan.analyzed);
        assert_eq!(plan.root.node_type, "Nested Loop");
        assert_eq!(plan.root.children[0].relation_name.as_deref(), Some("students"));
        assert_eq!(plan.root.children[0].index_name.as_deref(), Some("idx_group"));
        assert!((plan.root.total_cost - 3.25).abs() < 0.001);

        let analyze_tree = "\
-> Nested loop inner join  (cost=1.40 rows=4) (actual time=0.052..0.068 rows=4 loops=1)
    -> Table scan on g  (cost=0.45 rows=4) (actual time=0.025..0.031 rows=4 loops=1)
    -> Filter: (s.gpa >= 3.0)  (cost=0.25 rows=1) (actual time=0.005..0.008 rows=1 loops=4)
        -> Index lookup on s using idx_group (group_id=g.group_id)  (cost=0.25 rows=1) (actual time=0.004..0.006 rows=1 loops=4)";
        let analyzed = ExplainPlan::parse_mysql_analyze_tree("SELECT ...".to_owned(), analyze_tree)
            .expect("mysql analyze");
        assert!(analyzed.analyzed);
        assert_eq!(analyzed.node_count(), 4);
        assert_eq!(analyzed.root.children[1].children[0].index_name.as_deref(), Some("idx_group"));

        let sqlite_rows = vec![
            (2, 0, "SCAN students USING INDEX idx_gpa".to_owned()),
            (4, 0, "USE TEMP B-TREE FOR ORDER BY".to_owned()),
        ];
        let sl_plan = ExplainPlan::parse_sqlite_query_plan("SELECT * FROM students ORDER BY name".to_owned(), &sqlite_rows)
            .expect("sqlite plan");
        assert_eq!(sl_plan.root.children[0].node_type, "SCAN");
        assert_eq!(sl_plan.root.children[0].relation_name.as_deref(), Some("students"));
        assert_eq!(sl_plan.root.children[0].index_name.as_deref(), Some("idx_gpa"));
        assert_eq!(sl_plan.root.children[1].node_type, "USE TEMP B-TREE");

        let sl_res = QueryStatementResult {
            sql: "EXPLAIN QUERY PLAN SELECT * FROM students".to_owned(),
            columns: ["id", "parent", "notused", "detail"]
                .into_iter()
                .map(|n| ColumnMeta {
                    name: n.to_owned(),
                    data_type: "text".to_owned(),
                    udt_name: "text".to_owned(),
                    nullable: true,
                    default_expr: None,
                    is_primary_key: false,
                    is_unique: false,
                    comment: None,
                    foreign_key: None,
                })
                .collect(),
            rows: vec![
                vec![CellValue::Int(2), CellValue::Int(0), CellValue::Int(0), CellValue::Text("SEARCH students USING INDEX idx_gpa (gpa>?)".to_owned())],
            ],
            rows_affected: None,
            truncated: false,
            total_rows_seen: 1,
            elapsed_ms: 1,
            error: None,
        };
        let extracted = ExplainPlan::try_extract_from_result(&sl_res).expect("extract sqlite");
        assert_eq!(extracted.root.node_type, "SEARCH");
        assert_eq!(extracted.root.relation_name.as_deref(), Some("students"));
        assert_eq!(extracted.root.index_name.as_deref(), Some("idx_gpa"));
    }
}
