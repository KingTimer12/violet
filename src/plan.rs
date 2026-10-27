//! Query plans as a tree: what `EXPLAIN` returns in JSON on Postgres and
//! MySQL, and the rows of `EXPLAIN QUERY PLAN` on SQLite, read into one
//! shape the results pane can draw and flag problems in.

use serde_json::Value;
use violet_core::ResultSet;

/// Outer rows above which a nested loop that rescans its inner side is
/// flagged.
const LOOP_ROWS: f64 = 1000.0;

/// An estimate this many times off the actual count is flagged.
const MISESTIMATE: f64 = 10.0;

/// A reason to look at a node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Warning {
    /// Reads every row of a table.
    FullScan,
    /// Sorts through a file on disk instead of memory.
    DiskSort,
    /// Sorts the rows apart from any index.
    Filesort,
    /// Loops over many outer rows, rescanning the inner side each time.
    ExpensiveLoop,
    /// The planner's row estimate is far from what ran.
    Misestimate,
}

impl Warning {
    pub fn label(self) -> &'static str {
        match self {
            Self::FullScan => "Full scan",
            Self::DiskSort => "Disk sort",
            Self::Filesort => "Filesort",
            Self::ExpensiveLoop => "Expensive loop",
            Self::Misestimate => "Bad estimate",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Self::FullScan => "Reads the whole table. An index on the filtered column may help.",
            Self::DiskSort => "The sort spilled to disk. More work memory or an index may help.",
            Self::Filesort => "Rows are sorted apart from any index.",
            Self::ExpensiveLoop => {
                "Many outer rows each rescan the inner side. An index on the join column may help."
            }
            Self::Misestimate => "Estimated and actual rows differ a lot. Statistics may be stale.",
        }
    }
}

/// One step of a plan.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlanNode {
    pub label: String,
    /// The table or index the step reads, when it reads one.
    pub target: Option<String>,
    /// Conditions, join types and the like, one line each.
    pub details: Vec<String>,
    pub cost: Option<f64>,
    pub est_rows: Option<f64>,
    /// Rows the step produced; only `ANALYZE` knows.
    pub actual_rows: Option<f64>,
    /// Milliseconds spent in the step, with its children.
    pub time_ms: Option<f64>,
    pub loops: Option<f64>,
    pub warnings: Vec<Warning>,
    /// Joins its children by looping over the first one.
    pub join_loop: bool,
    pub children: Vec<PlanNode>,
}

impl PlanNode {
    fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            ..Self::default()
        }
    }

    /// What the node's bar measures: time when the plan ran, else cost.
    pub fn weight(&self) -> f64 {
        self.time_ms.or(self.cost).unwrap_or(0.0)
    }

    /// The rows the node handles, actual when known.
    fn rows(&self) -> Option<f64> {
        self.actual_rows.or(self.est_rows)
    }

    /// Flags what stands out in the node and below it.
    fn inspect(&mut self) {
        for child in &mut self.children {
            child.inspect();
        }
        if let (Some(est), Some(actual)) = (self.est_rows, self.actual_rows) {
            let (low, high) = (est.min(actual).max(1.0), est.max(actual).max(1.0));
            if high / low >= MISESTIMATE && high >= LOOP_ROWS / 10.0 {
                self.warnings.push(Warning::Misestimate);
            }
        }
        if self.join_loop
            && let [outer, inner @ ..] = self.children.as_slice()
            && outer.rows().is_some_and(|rows| rows >= LOOP_ROWS)
            && inner
                .iter()
                .any(|node| node.warnings.contains(&Warning::FullScan))
        {
            self.warnings.push(Warning::ExpensiveLoop);
        }
    }

    fn max_weight(&self) -> f64 {
        self.children
            .iter()
            .map(Self::max_weight)
            .fold(self.weight(), f64::max)
    }
}

/// A statement's plan.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Plan {
    pub roots: Vec<PlanNode>,
    pub planning_ms: Option<f64>,
    pub execution_ms: Option<f64>,
}

impl Plan {
    /// The heaviest node's weight, to scale bars against.
    pub fn max_weight(&self) -> f64 {
        self.roots
            .iter()
            .map(PlanNode::max_weight)
            .fold(0.0, f64::max)
    }

    /// Whether the plan carries actual rows and times.
    pub fn analyzed(&self) -> bool {
        self.execution_ms.is_some() || self.roots.iter().any(|node| node.time_ms.is_some())
    }

    /// Reads what a run of `EXPLAIN` returned, when it is a plan Violet
    /// knows: one cell of JSON, or SQLite's `id`, `parent`, `detail` rows.
    pub fn from_result(set: &ResultSet) -> Option<Self> {
        let names: Vec<&str> = set.columns.iter().map(|c| c.name.as_str()).collect();
        if let [_] = names.as_slice() {
            let text = set.rows.get(0, 0)??;
            return Self::from_json(text);
        }
        if names.first() == Some(&"id")
            && names.get(1) == Some(&"parent")
            && names.last() == Some(&"detail")
        {
            let last = names.len() - 1;
            let steps: Vec<(i64, i64, String)> = set
                .rows
                .iter()
                .filter_map(|row| {
                    let id = row.get(0)??.parse().ok()?;
                    let parent = row.get(1)??.parse().ok()?;
                    Some((id, parent, row.get(last)??.to_owned()))
                })
                .collect();
            return Self::from_sqlite(&steps);
        }
        None
    }

    /// Reads Postgres's `FORMAT JSON` or MySQL's `FORMAT=JSON`.
    pub fn from_json(text: &str) -> Option<Self> {
        let value: Value = serde_json::from_str(text).ok()?;
        let mut plan = match &value {
            Value::Array(items) => Self::postgres(items.first()?)?,
            Value::Object(map) if map.contains_key("query_block") => Self::mysql(&value)?,
            _ => return None,
        };
        for root in &mut plan.roots {
            root.inspect();
        }
        Some(plan)
    }

    fn postgres(top: &Value) -> Option<Self> {
        let root = postgres_node(top.get("Plan")?)?;
        Some(Self {
            roots: vec![root],
            planning_ms: number(top, "Planning Time"),
            execution_ms: number(top, "Execution Time"),
        })
    }

    fn mysql(value: &Value) -> Option<Self> {
        let block = mysql_block(value.get("query_block")?);
        Some(Self {
            roots: vec![block],
            ..Self::default()
        })
    }

    /// Reads the steps of `EXPLAIN QUERY PLAN` as `(id, parent, detail)`.
    pub fn from_sqlite(steps: &[(i64, i64, String)]) -> Option<Self> {
        if steps.is_empty() {
            return None;
        }
        let mut plan = Self {
            roots: sqlite_children(steps, 0),
            ..Self::default()
        };
        for root in &mut plan.roots {
            root.inspect();
        }
        Some(plan)
    }
}

fn number(value: &Value, key: &str) -> Option<f64> {
    let found = value.get(key)?;
    found.as_f64().or_else(|| found.as_str()?.parse().ok())
}

fn text(value: &Value, key: &str) -> Option<String> {
    Some(value.get(key)?.as_str()?.to_owned())
}

fn postgres_node(value: &Value) -> Option<PlanNode> {
    let mut node = PlanNode::new(text(value, "Node Type")?);
    node.target = text(value, "Relation Name").or_else(|| text(value, "CTE Name"));
    if let (Some(target), Some(alias)) = (&node.target, text(value, "Alias"))
        && *target != alias
    {
        node.target = Some(format!("{target} as {alias}"));
    }
    for (key, name) in [
        ("Index Name", "Index"),
        ("Join Type", "Join"),
        ("Index Cond", "Index cond"),
        ("Hash Cond", "Hash cond"),
        ("Merge Cond", "Merge cond"),
        ("Filter", "Filter"),
        ("Sort Method", "Sort method"),
    ] {
        if let Some(found) = text(value, key) {
            node.details.push(format!("{name}: {found}"));
        }
    }
    if let Some(Value::Array(keys)) = value.get("Sort Key") {
        let keys: Vec<&str> = keys.iter().filter_map(Value::as_str).collect();
        node.details.push(format!("Sort key: {}", keys.join(", ")));
    }
    if let Some(removed) = number(value, "Rows Removed by Filter").filter(|n| *n > 0.0) {
        node.details
            .push(format!("Rows removed by filter: {removed}"));
    }
    node.cost = number(value, "Total Cost");
    node.est_rows = number(value, "Plan Rows");
    node.actual_rows = number(value, "Actual Rows");
    node.time_ms = number(value, "Actual Total Time");
    node.loops = number(value, "Actual Loops");
    node.join_loop = node.label == "Nested Loop";
    let node_type = node.label.as_str();
    if node_type == "Seq Scan" {
        node.warnings.push(Warning::FullScan);
    }
    let spilled = value.get("Sort Space Type").and_then(Value::as_str) == Some("Disk")
        || value
            .get("Sort Method")
            .and_then(Value::as_str)
            .is_some_and(|method| method.contains("external"));
    if spilled {
        node.warnings.push(Warning::DiskSort);
    }
    if let Some(Value::Array(plans)) = value.get("Plans") {
        node.children = plans.iter().filter_map(postgres_node).collect();
    }
    Some(node)
}

/// A MySQL `query_block`, or the like, with what it holds as children.
fn mysql_block(block: &Value) -> PlanNode {
    let id = block.get("select_id").and_then(Value::as_i64);
    let mut node =
        PlanNode::new(id.map_or("Query block".to_owned(), |id| format!("Query block #{id}")));
    node.cost = block
        .get("cost_info")
        .and_then(|info| number(info, "query_cost"));
    node.children = mysql_children(block);
    node
}

/// The steps an object lists, in the order MySQL runs them.
fn mysql_children(object: &Value) -> Vec<PlanNode> {
    let Some(map) = object.as_object() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    // The object's own table comes before what hangs off it.
    if let Some(table) = map.get("table") {
        out.push(mysql_table(table));
    }
    if let Some(Value::Array(items)) = map.get("nested_loop") {
        let mut node = PlanNode::new("Nested loop");
        node.join_loop = true;
        node.children = items.iter().flat_map(mysql_children).collect();
        out.push(node);
    }
    for (key, label) in [
        ("ordering_operation", "Order"),
        ("grouping_operation", "Group"),
        ("duplicates_removal", "Distinct"),
        ("windowing", "Window"),
        ("buffer_result", "Buffer"),
    ] {
        if let Some(inner) = map.get(key) {
            let mut node = PlanNode::new(label);
            if inner.get("using_filesort").and_then(Value::as_bool) == Some(true) {
                node.warnings.push(Warning::Filesort);
            }
            if inner.get("using_temporary_table").and_then(Value::as_bool) == Some(true) {
                node.details.push("Uses a temporary table".to_owned());
            }
            node.children = mysql_children(inner);
            out.push(node);
        }
    }
    if let Some(union) = map.get("union_result") {
        let mut node = PlanNode::new("Union");
        if let Some(Value::Array(specs)) = union.get("query_specifications") {
            node.children = specs
                .iter()
                .filter_map(|spec| spec.get("query_block"))
                .map(mysql_block)
                .collect();
        }
        out.push(node);
    }
    for key in ["attached_subqueries", "optimized_away_subqueries"] {
        if let Some(Value::Array(subs)) = map.get(key) {
            for sub in subs {
                if let Some(block) = sub.get("query_block") {
                    out.push(mysql_block(block));
                }
            }
        }
    }
    out
}

fn mysql_table(table: &Value) -> PlanNode {
    let access = text(table, "access_type").unwrap_or_default();
    let mut node = PlanNode::new(match access.as_str() {
        "ALL" => "Table scan".to_owned(),
        "" => "Table".to_owned(),
        other => format!("Access {other}"),
    });
    node.target = text(table, "table_name");
    if let Some(key) = text(table, "key") {
        node.details.push(format!("Index: {key}"));
    }
    if let Some(Value::Array(keys)) = table.get("possible_keys")
        && table.get("key").is_none()
        && !keys.is_empty()
    {
        let keys: Vec<&str> = keys.iter().filter_map(Value::as_str).collect();
        node.details
            .push(format!("Unused candidates: {}", keys.join(", ")));
    }
    if let Some(condition) = text(table, "attached_condition") {
        node.details.push(format!("Filter: {condition}"));
    }
    node.est_rows =
        number(table, "rows_examined_per_scan").or_else(|| number(table, "rows_produced_per_join"));
    node.cost = table
        .get("cost_info")
        .and_then(|info| number(info, "prefix_cost").or_else(|| number(info, "read_cost")));
    if access == "ALL" {
        node.warnings.push(Warning::FullScan);
    }
    if let Some(sub) = table
        .get("materialized_from_subquery")
        .and_then(|sub| sub.get("query_block"))
    {
        node.children.push(mysql_block(sub));
    }
    node
}

/// The steps whose parent is `parent`, each with its own children.
fn sqlite_children(steps: &[(i64, i64, String)], parent: i64) -> Vec<PlanNode> {
    steps
        .iter()
        .filter(|(id, owner, _)| *owner == parent && *id != parent)
        .map(|(id, _, detail)| {
            let mut node = PlanNode::new(detail.as_str());
            if sqlite_scans_table(detail) {
                node.warnings.push(Warning::FullScan);
            }
            if detail.starts_with("USE TEMP B-TREE FOR") {
                node.warnings.push(Warning::Filesort);
            }
            node.children = sqlite_children(steps, *id);
            node
        })
        .collect()
}

/// `SCAN t` reads the whole table; with `USING INDEX` it reads an index,
/// and constants and subqueries read no table.
fn sqlite_scans_table(detail: &str) -> bool {
    detail.starts_with("SCAN ")
        && !detail.contains(" USING ")
        && !detail.starts_with("SCAN CONSTANT")
        && !detail.starts_with("SCAN SUBQUERY")
}

#[cfg(test)]
mod tests {
    use super::*;

    const POSTGRES: &str = r#"[{
      "Plan": {
        "Node Type": "Nested Loop", "Join Type": "Inner",
        "Total Cost": 5000.5, "Plan Rows": 10, "Actual Rows": 5000,
        "Actual Loops": 1, "Actual Total Time": 120.5,
        "Plans": [
          {"Node Type": "Seq Scan", "Relation Name": "orders", "Alias": "o",
           "Total Cost": 100.0, "Plan Rows": 4000, "Actual Rows": 4000,
           "Actual Loops": 1, "Actual Total Time": 5.0,
           "Filter": "(total > 10)", "Rows Removed by Filter": 20},
          {"Node Type": "Seq Scan", "Relation Name": "items",
           "Total Cost": 40.0, "Plan Rows": 1, "Actual Rows": 1,
           "Actual Loops": 4000, "Actual Total Time": 90.0}
        ]
      },
      "Planning Time": 0.4, "Execution Time": 121.0
    }]"#;

    #[test]
    fn reads_postgres_analyze() {
        let plan = Plan::from_json(POSTGRES).expect("a plan");
        assert!(plan.analyzed());
        assert_eq!(plan.planning_ms, Some(0.4));
        assert_eq!(plan.execution_ms, Some(121.0));
        let root = &plan.roots[0];
        assert_eq!(root.label, "Nested Loop");
        assert_eq!(root.children.len(), 2);
        let orders = &root.children[0];
        assert_eq!(orders.target.as_deref(), Some("orders as o"));
        assert_eq!(orders.est_rows, Some(4000.0));
        assert_eq!(orders.actual_rows, Some(4000.0));
        assert!(
            orders
                .details
                .contains(&"Rows removed by filter: 20".to_owned())
        );
        assert_eq!(plan.max_weight(), 120.5);
    }

    #[test]
    fn flags_postgres_trouble() {
        let plan = Plan::from_json(POSTGRES).expect("a plan");
        let root = &plan.roots[0];
        assert_eq!(root.children[0].warnings, [Warning::FullScan]);
        // Ten estimated, 5000 produced; a big outer side rescanning a table.
        assert!(root.warnings.contains(&Warning::Misestimate));
        assert!(root.warnings.contains(&Warning::ExpensiveLoop));
    }

    #[test]
    fn flags_disk_sort_and_leaves_index_scans_alone() {
        let plan = Plan::from_json(
            r#"[{"Plan": {"Node Type": "Sort", "Sort Method": "external merge",
                "Sort Space Type": "Disk", "Sort Key": ["a", "b"], "Total Cost": 9,
                "Plan Rows": 5,
                "Plans": [{"Node Type": "Index Scan", "Index Name": "t_pkey",
                           "Relation Name": "t", "Total Cost": 1, "Plan Rows": 5}]}}]"#,
        )
        .expect("a plan");
        assert!(!plan.analyzed());
        let sort = &plan.roots[0];
        assert_eq!(sort.warnings, [Warning::DiskSort]);
        assert!(sort.details.contains(&"Sort key: a, b".to_owned()));
        assert!(sort.children[0].warnings.is_empty());
        assert_eq!(plan.max_weight(), 9.0);
    }

    const MYSQL: &str = r#"{"query_block": {
      "select_id": 1, "cost_info": {"query_cost": "2500.10"},
      "ordering_operation": {
        "using_filesort": true,
        "nested_loop": [
          {"table": {"table_name": "a", "access_type": "ALL",
                     "possible_keys": ["idx_a"], "rows_examined_per_scan": 5000,
                     "cost_info": {"prefix_cost": "500.0"}}},
          {"table": {"table_name": "b", "access_type": "ALL",
                     "rows_examined_per_scan": 10,
                     "attached_condition": "(b.a_id = a.id)"}}
        ]
      }}}"#;

    #[test]
    fn reads_and_flags_mysql() {
        let plan = Plan::from_json(MYSQL).expect("a plan");
        assert!(!plan.analyzed());
        let block = &plan.roots[0];
        assert_eq!(block.label, "Query block #1");
        assert_eq!(block.cost, Some(2500.10));
        let order = &block.children[0];
        assert_eq!(order.warnings, [Warning::Filesort]);
        let nested = &order.children[0];
        assert_eq!(nested.label, "Nested loop");
        assert_eq!(nested.children[0].target.as_deref(), Some("a"));
        assert_eq!(nested.children[0].cost, Some(500.0));
        assert!(
            nested.children[0]
                .details
                .contains(&"Unused candidates: idx_a".to_owned())
        );
        assert_eq!(nested.children[1].warnings, [Warning::FullScan]);
        assert!(nested.warnings.contains(&Warning::ExpensiveLoop));
    }

    #[test]
    fn reads_sqlite_tree() {
        let steps = [
            (2, 0, "SCAN a".to_owned()),
            (4, 0, "SEARCH b USING INDEX b_a (a_id=?)".to_owned()),
            (9, 0, "USE TEMP B-TREE FOR ORDER BY".to_owned()),
            (11, 9, "SCAN c USING COVERING INDEX c_i".to_owned()),
        ];
        let plan = Plan::from_sqlite(&steps).expect("a plan");
        assert_eq!(plan.roots.len(), 3);
        assert_eq!(plan.roots[0].warnings, [Warning::FullScan]);
        assert!(plan.roots[1].warnings.is_empty());
        assert_eq!(plan.roots[2].warnings, [Warning::Filesort]);
        assert!(plan.roots[2].children[0].warnings.is_empty());
        assert!(Plan::from_sqlite(&[]).is_none());
    }

    #[test]
    fn ignores_what_is_not_a_plan() {
        assert!(Plan::from_json("not json").is_none());
        assert!(Plan::from_json(r#"{"a": 1}"#).is_none());
        assert!(Plan::from_json("[]").is_none());
        assert!(Plan::from_json(r#"[{"Plan": {"no": "type"}}]"#).is_none());
    }
}
