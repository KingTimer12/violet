//! Runs of the same query set side by side: how long they took over time,
//! and how an explain plan changed between two runs.

use violet_core::ports::HistoryEntry;

use crate::plan::{Plan, PlanNode};

/// Text to match runs of a query by: spaces collapsed, no trailing `;`.
pub fn normalize(sql: &str) -> String {
    sql.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_end_matches(';')
        .trim_end()
        .to_owned()
}

/// How the runs of one query went, oldest first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunStats {
    /// Milliseconds of each run that succeeded.
    pub times: Vec<u64>,
    pub failed: usize,
}

impl RunStats {
    /// The runs of `entry`'s query on its connection, `entry`'s own
    /// included, among `history`.
    pub fn of(history: &[HistoryEntry], entry: &HistoryEntry) -> Self {
        let wanted = normalize(&entry.sql);
        let mut times = Vec::new();
        let mut failed = 0;
        for run in history {
            if run.connection != entry.connection || normalize(&run.sql) != wanted {
                continue;
            }
            if run.error.is_some() {
                failed += 1;
            } else {
                times.push(run.elapsed_ms);
            }
        }
        Self { times, failed }
    }

    pub fn last(&self) -> Option<u64> {
        self.times.last().copied()
    }

    pub fn best(&self) -> Option<u64> {
        self.times.iter().min().copied()
    }

    pub fn worst(&self) -> Option<u64> {
        self.times.iter().max().copied()
    }

    pub fn median(&self) -> Option<u64> {
        let mut sorted = self.times.clone();
        sorted.sort_unstable();
        sorted.get(sorted.len() / 2).copied()
    }

    /// How much slower (positive) or faster the last run was than the one
    /// before it, as a fraction of that one.
    pub fn change(&self) -> Option<f64> {
        let [.., before, last] = self.times.as_slice() else {
            return None;
        };
        (*before > 0).then(|| (*last as f64 - *before as f64) / *before as f64)
    }
}

/// A figure in two runs.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Delta {
    pub old: Option<f64>,
    pub new: Option<f64>,
}

impl Delta {
    /// How much more (positive) or less the new figure is, as a fraction
    /// of the old; `None` when either is missing or the old one is zero.
    pub fn change(self) -> Option<f64> {
        let (old, new) = (self.old?, self.new?);
        (old > 0.0).then(|| (new - old) / old)
    }
}

/// What became of a step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    Same,
    /// The step is another kind now, such as a scan turned index scan.
    Replaced,
    Added,
    Removed,
}

/// One step of the comparison.
#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    pub depth: usize,
    pub change: Change,
    pub label: String,
    pub target: Option<String>,
    /// What the step was, when it was another.
    pub was: Option<String>,
    pub cost: Delta,
    pub rows: Delta,
    pub time: Delta,
}

/// The steps of `new` against those of `old`, paired by their place in the
/// tree. Steps only one has are `Added` or `Removed`.
pub fn plans(old: &Plan, new: &Plan) -> Vec<Line> {
    let mut lines = Vec::new();
    pair(&old.roots, &new.roots, 0, &mut lines);
    lines
}

fn figures(old: Option<&PlanNode>, new: Option<&PlanNode>) -> (Delta, Delta, Delta) {
    let rows = |node: &PlanNode| node.actual_rows.or(node.est_rows);
    (
        Delta {
            old: old.and_then(|n| n.cost),
            new: new.and_then(|n| n.cost),
        },
        Delta {
            old: old.and_then(rows),
            new: new.and_then(rows),
        },
        Delta {
            old: old.and_then(|n| n.time_ms),
            new: new.and_then(|n| n.time_ms),
        },
    )
}

fn pair(old: &[PlanNode], new: &[PlanNode], depth: usize, lines: &mut Vec<Line>) {
    for index in 0..old.len().max(new.len()) {
        let (before, after) = (old.get(index), new.get(index));
        let (cost, rows, time) = figures(before, after);
        let shown = after.or(before);
        let Some(node) = shown else { continue };
        let change = match (before, after) {
            (Some(_), None) => Change::Removed,
            (None, Some(_)) => Change::Added,
            (Some(a), Some(b)) if a.label != b.label || a.target != b.target => Change::Replaced,
            _ => Change::Same,
        };
        lines.push(Line {
            depth,
            change,
            label: node.label.clone(),
            target: node.target.clone(),
            was: (change == Change::Replaced)
                .then(|| {
                    before.map(|a| match &a.target {
                        Some(target) => format!("{} {target}", a.label),
                        None => a.label.clone(),
                    })
                })
                .flatten(),
            cost,
            rows,
            time,
        });
        pair(
            before.map_or(&[], |n| n.children.as_slice()),
            after.map_or(&[], |n| n.children.as_slice()),
            depth + 1,
            lines,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use violet_core::ConnectionId;

    fn run(sql: &str, ms: u64, error: bool) -> HistoryEntry {
        HistoryEntry {
            at: 0,
            connection: Some(ConnectionId(uuid::Uuid::nil())),
            source: String::new(),
            sql: sql.into(),
            elapsed_ms: ms,
            rows: None,
            error: error.then(|| "boom".into()),
        }
    }

    #[test]
    fn groups_runs_of_the_same_query() {
        let history = [
            run("SELECT  1;", 100, false),
            run("select 1", 90, false),
            run("SELECT 1", 0, true),
            run("SELECT 2", 5, false),
            run("SELECT 1", 150, false),
        ];
        let stats = RunStats::of(&history, &history[0]);
        // Case matters: `select 1` is another text.
        assert_eq!(stats.times, [100, 150]);
        assert_eq!(stats.failed, 1);
        assert_eq!(
            (stats.best(), stats.worst(), stats.last()),
            (Some(100), Some(150), Some(150))
        );
        assert_eq!(stats.change(), Some(0.5));
        assert_eq!(normalize("SELECT\n  a ,b ;"), "SELECT a ,b");
    }

    #[test]
    fn a_single_run_has_no_change() {
        let history = [run("SELECT 1", 10, false)];
        let stats = RunStats::of(&history, &history[0]);
        assert_eq!(stats.change(), None);
        assert_eq!(stats.median(), Some(10));
    }

    fn scan(label: &str, target: &str, cost: f64, time: Option<f64>) -> PlanNode {
        PlanNode {
            label: label.into(),
            target: Some(target.into()),
            cost: Some(cost),
            time_ms: time,
            ..PlanNode::default()
        }
    }

    #[test]
    fn compares_plans_step_by_step() {
        let old = Plan {
            roots: vec![PlanNode {
                label: "Nested Loop".into(),
                cost: Some(1000.0),
                children: vec![
                    scan("Seq Scan", "orders", 800.0, Some(40.0)),
                    scan("Seq Scan", "items", 100.0, None),
                ],
                ..PlanNode::default()
            }],
            ..Plan::default()
        };
        let new = Plan {
            roots: vec![PlanNode {
                label: "Nested Loop".into(),
                cost: Some(500.0),
                children: vec![scan("Index Scan", "orders", 80.0, Some(4.0))],
                ..PlanNode::default()
            }],
            ..Plan::default()
        };
        let lines = plans(&old, &new);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].change, Change::Same);
        assert_eq!(lines[0].cost.change(), Some(-0.5));
        assert_eq!(lines[1].change, Change::Replaced);
        assert_eq!(lines[1].was.as_deref(), Some("Seq Scan orders"));
        assert_eq!(lines[1].time.change(), Some(-0.9));
        assert_eq!((lines[2].change, lines[2].depth), (Change::Removed, 1));
        assert_eq!(lines[2].label, "Seq Scan");
        assert_eq!(Delta::default().change(), None);
    }
}
