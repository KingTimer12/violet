//! Open tabs: SQL editors and table data views.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use violet_app::{ExecOptions, RequestId};
use violet_core::{ConnectionId, DbError, OrderBy, QueryOutput};

use super::editor::{Highlighter, Popup};

pub type TabId = u64;

/// What a run sent to the server, to place errors back in the editor.
#[derive(Clone, Debug)]
pub struct Executed {
    /// Byte offset of the user's text in the editor.
    pub offset: usize,
    pub sql: String,
    /// Characters Violet put before the user's text in `sql`, such as
    /// `EXPLAIN `.
    pub shift: usize,
}

#[derive(Debug)]
pub struct Running {
    pub request: RequestId,
    pub started: Instant,
    pub executed: Executed,
    pub cancelling: bool,
    /// Fetching the next page: rows are added to the ones shown, which
    /// stay on screen meanwhile.
    pub append: bool,
}

/// What a tab is for.
#[derive(Debug)]
pub enum TabKind {
    /// A SQL editor.
    Script,
    /// A relation's rows, as DBeaver's data editor shows them.
    Data(DataView),
}

/// A data tab's relation and how it is filtered and sorted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataView {
    pub schema: String,
    pub relation: String,
    /// The filter box as typed.
    pub filter: String,
    /// The filter the rows shown were read with.
    pub applied: String,
    pub order: Option<OrderBy>,
    /// For a key-value store's key, its type (`hash`, `zset`...): what it
    /// reads with and how its rows are written back. The relation is then
    /// the key, and the filter a glob on its fields or members.
    pub subtype: Option<String>,
}

impl DataView {
    pub fn new(schema: String, relation: String) -> Self {
        Self {
            schema,
            relation,
            filter: String::new(),
            applied: String::new(),
            order: None,
            subtype: None,
        }
    }

    /// Sorting by `column` cycles ascending, descending, unsorted.
    pub fn cycle_order(&mut self, column: &str) {
        self.order = match &self.order {
            Some(order) if order.column == column && !order.descending => Some(OrderBy {
                column: column.to_owned(),
                descending: true,
            }),
            Some(order) if order.column == column => None,
            _ => Some(OrderBy {
                column: column.to_owned(),
                descending: false,
            }),
        };
    }
}

/// A plan an explain returned, and the statement that asked for it.
#[derive(Clone, Debug)]
pub struct PlanRun {
    pub sql: String,
    pub plan: crate::plan::Plan,
}

/// Plans kept per tab.
const PLAN_RUNS: usize = 8;

/// The plan the last explain of the same statement returned before the
/// latest of `runs`.
pub fn previous_plan(runs: &[PlanRun]) -> Option<&crate::plan::Plan> {
    let (latest, before) = runs.split_last()?;
    before
        .iter()
        .rev()
        .find(|run| run.sql == latest.sql)
        .map(|run| &run.plan)
}

impl QueryTab {
    /// Keeps the plan of an explain that just ran.
    pub fn keep_plan(&mut self, sql: &str, sets: &[violet_core::ResultSet]) {
        if !crate::plan::is_explain(sql) {
            return;
        }
        if let Some(plan) = sets.iter().find_map(crate::plan::Plan::from_result) {
            self.plan_runs.push(PlanRun {
                sql: sql.to_owned(),
                plan,
            });
            if self.plan_runs.len() > PLAN_RUNS {
                self.plan_runs.remove(0);
            }
        }
    }
}

/// A result read a page at a time.
#[derive(Debug)]
pub struct Paging {
    /// The single statement being paged, as the user wrote it.
    pub base: String,
    /// How the first page was run; later pages run the same way.
    pub options: ExecOptions,
    pub loaded: u64,
    pub has_more: bool,
    pub total: Option<Count>,
}

/// A `count(*)` of a paged result.
#[derive(Debug)]
pub enum Count {
    Running(RequestId),
    Done(u64),
    Failed(String),
}

#[derive(Debug)]
pub enum Outcome {
    Done { output: QueryOutput },
    Failed { error: DbError, executed: Executed },
}

/// One editor tab.
pub struct QueryTab {
    pub id: TabId,
    pub title: String,
    pub connection: Option<ConnectionId>,
    pub sql: String,
    /// The editor's selection as a byte range; empty when it is only a
    /// cursor.
    pub selection: Range<usize>,
    pub running: Option<Running>,
    pub outcome: Option<Outcome>,
    pub active_set: usize,
    /// The cells picked in the grid.
    pub grid_selection: super::selection::Selection,
    pub kind: TabKind,
    /// Set while the shown result is read a page at a time.
    pub paging: Option<Paging>,
    /// The last run, for refreshing it.
    pub last_run: Option<(Executed, ExecOptions)>,
    pub highlighter: Highlighter,
    /// Completion, while it shows.
    pub popup: Popup,
    /// The problems found in the text, marked in the editor.
    pub diagnostics: crate::diagnostics::Tracker,
    /// The stops Tab still visits in an inserted snippet.
    pub snippet: Option<crate::editing::Snippet>,
    /// How an explain's result is shown.
    pub plan_view: super::plan_view::View,
    /// The plans this tab's explains returned, oldest first, to compare.
    pub plan_runs: Vec<PlanRun>,
    /// Performance hints on the text.
    pub hints: super::hints_flow::Hints,
    /// When the shown result reached the app, until it is on screen.
    pub received: Option<Instant>,
    /// How long the shown result took to reach the screen.
    pub render: Option<Duration>,
    /// The connection's round trip when the shown result was read.
    pub rtt: Option<Duration>,
    /// What the tab's explain asks of the server.
    pub explain: crate::dialect::Explain,
    /// The file the script is saved in, and its text as last read or
    /// written there.
    pub file: Option<ScriptFile>,
    /// The table the shown rows were read from, when they map back to it.
    pub source: Option<crate::sql::TableRef>,
    /// Cells changed in the grid and not written yet, by row and column:
    /// the new value, `None` for NULL. Rows past the result's are the
    /// added ones.
    pub edits: BTreeMap<(usize, usize), Option<String>>,
    /// Rows added in the grid and not inserted yet. They follow the
    /// result's rows, their values in `edits`; a column without one takes
    /// its default.
    pub added: usize,
    /// Rows of the result to delete.
    pub deleted: BTreeSet<usize>,
    /// A row the grid scrolls to on its next frame.
    pub reveal_row: Option<usize>,
    /// The cell being edited in place in the grid.
    pub inline: Option<InlineCell>,
    /// The editor's find bar, when open.
    pub find: Option<super::editor::FindBar>,
    /// A query's rows sorted by a column (index, descending), and the query
    /// as written, which the sorted one wraps.
    pub sort: Option<(usize, bool)>,
    pub unsorted: Option<String>,
    /// Reads the rows again on this interval, and when it is next due.
    pub auto_refresh: Option<(std::time::Duration, std::time::Instant)>,
}

/// A grid cell edited in place.
pub struct InlineCell {
    pub row: usize,
    pub column: usize,
    pub text: String,
    /// What it started with: left as it was, it changes nothing.
    pub initial: String,
    /// Whether the field still has to take the keyboard.
    pub focus: bool,
}

impl InlineCell {
    pub fn new(row: usize, column: usize, text: &str) -> Self {
        Self {
            row,
            column,
            text: text.to_owned(),
            initial: text.to_owned(),
            focus: true,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ScriptFile {
    pub path: PathBuf,
    pub saved: String,
}

impl QueryTab {
    pub fn data(&self) -> Option<&DataView> {
        match &self.kind {
            TabKind::Data(view) => Some(view),
            TabKind::Script => None,
        }
    }

    /// Whether the grid has changes not written yet: cells, added rows or
    /// rows to delete.
    pub fn has_changes(&self) -> bool {
        !self.edits.is_empty() || self.added > 0 || !self.deleted.is_empty()
    }

    /// Forgets the grid's changes.
    pub fn clear_changes(&mut self) {
        self.edits.clear();
        self.added = 0;
        self.deleted.clear();
        self.inline = None;
    }

    /// Whether the script has changes its file does not.
    pub fn unsaved(&self) -> bool {
        self.file
            .as_ref()
            .is_some_and(|file| file.saved != self.sql)
    }

    pub fn data_mut(&mut self) -> Option<&mut DataView> {
        match &mut self.kind {
            TabKind::Data(view) => Some(view),
            TabKind::Script => None,
        }
    }

    /// Where the last error points in the editor, as a byte offset.
    pub fn error_offset(&self) -> Option<usize> {
        let Some(Outcome::Failed { error, executed }) = &self.outcome else {
            return None;
        };
        let position = usize::try_from(error.position?).ok()?;
        let mut chars = executed.sql.char_indices().skip(executed.shift);
        let start = chars
            .clone()
            .next()
            .map_or(executed.sql.len(), |(at, _)| at);
        let within = chars
            .nth(position.saturating_sub(executed.shift + 1))
            .map_or(executed.sql.len(), |(at, _)| at);
        Some(executed.offset + within.max(start) - start)
    }
}

/// The SQL editors open when Violet closed, to open them again.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SavedSession {
    pub tabs: Vec<SavedTab>,
    pub active: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SavedTab {
    pub title: String,
    pub connection: Option<ConnectionId>,
    pub sql: String,
    /// The file the script is saved in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
}

#[derive(Default)]
pub struct Workspace {
    pub tabs: Vec<QueryTab>,
    pub active: Option<TabId>,
    next: TabId,
}

impl Workspace {
    pub fn open(
        &mut self,
        title: String,
        connection: Option<ConnectionId>,
        sql: String,
        kind: TabKind,
    ) -> TabId {
        self.next += 1;
        let id = self.next;
        self.tabs.push(QueryTab {
            id,
            title,
            connection,
            selection: sql.len()..sql.len(),
            sql,
            running: None,
            outcome: None,
            active_set: 0,
            grid_selection: super::selection::Selection::default(),
            kind,
            paging: None,
            last_run: None,
            highlighter: Highlighter::default(),
            popup: Popup::default(),
            diagnostics: crate::diagnostics::Tracker::default(),
            snippet: None,
            plan_view: super::plan_view::View::default(),
            plan_runs: Vec::new(),
            hints: super::hints_flow::Hints::default(),
            received: None,
            render: None,
            rtt: None,
            explain: crate::dialect::Explain::default(),
            file: None,
            source: None,
            edits: BTreeMap::new(),
            added: 0,
            deleted: BTreeSet::new(),
            reveal_row: None,
            inline: None,
            find: None,
            sort: None,
            unsorted: None,
            auto_refresh: None,
        });
        self.active = Some(id);
        id
    }

    /// The SQL editors, as they are now. Data tabs are left out: they read
    /// the server again when opened.
    pub fn save(&self) -> SavedSession {
        let scripts: Vec<&QueryTab> = self
            .tabs
            .iter()
            .filter(|tab| matches!(tab.kind, TabKind::Script))
            .collect();
        SavedSession {
            active: scripts.iter().position(|tab| Some(tab.id) == self.active),
            tabs: scripts
                .into_iter()
                .map(|tab| SavedTab {
                    title: tab.title.clone(),
                    connection: tab.connection,
                    sql: tab.sql.clone(),
                    path: tab.file.as_ref().map(|file| file.path.clone()),
                })
                .collect(),
        }
    }

    /// Opens the editors of a saved session; `exists` says which
    /// connections are still there.
    pub fn restore(&mut self, session: SavedSession, exists: impl Fn(ConnectionId) -> bool) {
        let mut ids = Vec::new();
        for tab in session.tabs {
            let connection = tab.connection.filter(|id| exists(*id));
            let id = self.open(tab.title, connection, tab.sql, TabKind::Script);
            if let (Some(path), Some(opened)) = (tab.path, self.get_mut(id)) {
                // Compared with the file once it is read.
                opened.file = Some(ScriptFile {
                    path,
                    saved: opened.sql.clone(),
                });
            }
            ids.push(id);
        }
        self.active = session
            .active
            .and_then(|index| ids.get(index).copied())
            .or_else(|| ids.last().copied());
    }

    /// A title for a new editor: `Script 3`.
    pub fn next_script_title(&self) -> String {
        (1..)
            .map(|n| format!("Script {n}"))
            .find(|title| !self.tabs.iter().any(|tab| tab.title == *title))
            .unwrap_or_default()
    }

    /// The editor open on the script at `path`.
    pub fn script_tab(&self, path: &std::path::Path) -> Option<TabId> {
        self.tabs
            .iter()
            .find(|tab| tab.file.as_ref().is_some_and(|file| file.path == path))
            .map(|tab| tab.id)
    }

    pub fn get(&self, id: TabId) -> Option<&QueryTab> {
        self.tabs.iter().find(|tab| tab.id == id)
    }

    pub fn get_mut(&mut self, id: TabId) -> Option<&mut QueryTab> {
        self.tabs.iter_mut().find(|tab| tab.id == id)
    }

    pub fn active(&self) -> Option<&QueryTab> {
        let id = self.active?;
        self.tabs.iter().find(|tab| tab.id == id)
    }

    /// Closes a tab, activating its neighbour. Returns the closed tab.
    pub fn close(&mut self, id: TabId) -> Option<QueryTab> {
        let index = self.tabs.iter().position(|tab| tab.id == id)?;
        let tab = self.tabs.remove(index);
        if self.active == Some(id) {
            self.active = self
                .tabs
                .get(index)
                .or_else(|| self.tabs.get(index.wrapping_sub(1)))
                .map(|tab| tab.id);
        }
        Some(tab)
    }

    /// The data tab already showing `schema.relation` on `connection`.
    pub fn data_tab(
        &self,
        connection: ConnectionId,
        schema: &str,
        relation: &str,
    ) -> Option<TabId> {
        self.tabs
            .iter()
            .find(|tab| {
                tab.connection == Some(connection)
                    && tab
                        .data()
                        .is_some_and(|view| view.schema == schema && view.relation == relation)
            })
            .map(|tab| tab.id)
    }

    pub fn by_count(&mut self, request: RequestId) -> Option<&mut QueryTab> {
        self.tabs.iter_mut().find(|tab| {
            matches!(
                tab.paging.as_ref().and_then(|paging| paging.total.as_ref()),
                Some(Count::Running(running)) if *running == request
            )
        })
    }

    pub fn by_request(&mut self, request: RequestId) -> Option<&mut QueryTab> {
        self.tabs.iter_mut().find(|tab| {
            tab.running
                .as_ref()
                .is_some_and(|running| running.request == request)
        })
    }
}

/// `850 ms`, `2.31 s`, `1 m 05 s`.
pub fn format_duration(duration: Duration) -> String {
    let millis = duration.as_millis();
    if millis < 1_000 {
        format!("{millis} ms")
    } else if millis < 60_000 {
        format!("{:.2} s", duration.as_secs_f64())
    } else {
        let seconds = duration.as_secs();
        format!("{} m {:02} s", seconds / 60, seconds % 60)
    }
}

/// `12345` as `12,345`.
pub fn thousands(value: usize) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_previous_plan_is_of_the_same_statement() {
        let run = |sql: &str, cost: f64| PlanRun {
            sql: sql.into(),
            plan: crate::plan::Plan {
                roots: vec![crate::plan::PlanNode {
                    cost: Some(cost),
                    ..crate::plan::PlanNode::default()
                }],
                ..crate::plan::Plan::default()
            },
        };
        let cost = |runs: &[PlanRun]| previous_plan(runs).and_then(|plan| plan.roots[0].cost);
        assert_eq!(cost(&[]), None);
        assert_eq!(cost(&[run("a", 1.0)]), None);
        assert_eq!(
            cost(&[run("a", 1.0), run("b", 2.0), run("a", 3.0)]),
            Some(1.0)
        );
        assert_eq!(cost(&[run("a", 1.0), run("b", 2.0)]), None);
    }

    #[test]
    fn closing_the_active_tab_activates_a_neighbour() {
        let mut workspace = Workspace::default();
        let a = workspace.open("A".into(), None, String::new(), TabKind::Script);
        let b = workspace.open("B".into(), None, String::new(), TabKind::Script);
        let c = workspace.open("C".into(), None, String::new(), TabKind::Script);
        workspace.active = Some(b);
        workspace.close(b);
        assert_eq!(workspace.active, Some(c));
        workspace.close(c);
        assert_eq!(workspace.active, Some(a));
        workspace.close(a);
        assert_eq!(workspace.active, None);
    }

    #[test]
    fn editors_survive_a_restart() {
        let mut workspace = Workspace::default();
        let kept = ConnectionId::new();
        let gone = ConnectionId::new();
        workspace.open("A".into(), Some(kept), "SELECT 1".into(), TabKind::Script);
        let b = workspace.open("B".into(), Some(gone), "SELECT 2".into(), TabKind::Script);
        workspace.open(
            "t".into(),
            Some(kept),
            String::new(),
            TabKind::Data(DataView::new("s".into(), "t".into())),
        );
        workspace.active = Some(b);
        let saved = workspace.save();
        assert_eq!(saved.tabs.len(), 2);
        assert_eq!(saved.active, Some(1));

        let mut restored = Workspace::default();
        restored.restore(saved, |id| id == kept);
        assert_eq!(restored.tabs.len(), 2);
        assert_eq!(
            restored.active().map(|tab| tab.sql.as_str()),
            Some("SELECT 2")
        );
        assert_eq!(restored.tabs[0].connection, Some(kept));
        assert_eq!(restored.tabs[1].connection, None);
    }

    #[test]
    fn sorting_cycles_through_both_directions() {
        let mut view = DataView::new("s".into(), "t".into());
        view.cycle_order("a");
        assert_eq!(
            view.order
                .as_ref()
                .map(|o| (o.column.as_str(), o.descending)),
            Some(("a", false))
        );
        view.cycle_order("a");
        assert_eq!(view.order.as_ref().map(|o| o.descending), Some(true));
        view.cycle_order("a");
        assert_eq!(view.order, None);
        view.cycle_order("a");
        view.cycle_order("b");
        assert_eq!(
            view.order
                .as_ref()
                .map(|o| (o.column.as_str(), o.descending)),
            Some(("b", false))
        );
    }

    #[test]
    fn numbers_and_durations_read_well() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(1234567), "1,234,567");
        assert_eq!(format_duration(Duration::from_millis(42)), "42 ms");
        assert_eq!(format_duration(Duration::from_millis(2_310)), "2.31 s");
        assert_eq!(format_duration(Duration::from_secs(65)), "1 m 05 s");
    }

    #[test]
    fn error_positions_map_into_the_editor() {
        let mut workspace = Workspace::default();
        let id = workspace.open(
            "A".into(),
            None,
            "SELECT 1;\nSELEC 2".into(),
            TabKind::Script,
        );
        let tab = workspace.get_mut(id).expect("tab");
        tab.outcome = Some(Outcome::Failed {
            error: DbError {
                position: Some(1),
                ..DbError::new(violet_core::ErrorKind::Query, "syntax")
            },
            executed: Executed {
                offset: 10,
                sql: "SELEC 2".into(),
                shift: 0,
            },
        });
        assert_eq!(tab.error_offset(), Some(10));
        // Behind `EXPLAIN `, position 9 is the user's first character.
        tab.outcome = Some(Outcome::Failed {
            error: DbError {
                position: Some(10),
                ..DbError::new(violet_core::ErrorKind::Query, "syntax")
            },
            executed: Executed {
                offset: 10,
                sql: "EXPLAIN SELEC 2".into(),
                shift: 8,
            },
        });
        assert_eq!(tab.error_offset(), Some(11));
    }
}
