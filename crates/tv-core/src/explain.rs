use serde_json::Value;

use crate::engine::QueryStatementResult;

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
    let trimmed = statement.trim().trim_end_matches(';').trim();
    if analyze {
        format!("EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) {trimmed}")
    } else {
        format!("EXPLAIN (FORMAT JSON) {trimmed}")
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

    /// Detects and parses `EXPLAIN (FORMAT JSON)` output returned directly inside a SQL editor result set.
    pub fn try_extract_from_result(res: &QueryStatementResult) -> Option<Self> {
        if res.columns.len() != 1 || res.rows.is_empty() {
            return None;
        }
        if !res.columns[0].name.eq_ignore_ascii_case("QUERY PLAN") {
            return None;
        }
        let mut combined = String::new();
        for (i, row) in res.rows.iter().enumerate() {
            if i > 0 {
                combined.push('\n');
            }
            if let Some(cell) = row.first()
                && !cell.is_null()
            {
                combined.push_str(&cell.copy_text());
            }
        }
        let trimmed = combined.trim();
        if !trimmed.starts_with('[') && !trimmed.starts_with('{') {
            return None;
        }
        Self::parse_json(res.sql.clone(), false, trimmed).ok()
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

fn str_field(obj: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    obj.get(key).and_then(Value::as_str).map(str::to_owned)
}
