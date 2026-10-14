//! Open tabs: SQL editors and table data views.

use std::ops::Range;
use std::time::{Duration, Instant};

use violet_app::{ExecOptions, RequestId};
use violet_core::{ConnectionId, DbError, OrderBy, QueryOutput};

use super::editor::Highlighter;
use super::results::CellCache;

pub type TabId = u64;

/// What a run sent to the server, to place errors back in the editor.
#[derive(Clone, Debug)]
pub struct Executed {
    /// Byte offset of `sql` in the editor's text.
    pub offset: usize,
    pub sql: String,
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
}

impl DataView {
    pub fn new(schema: String, relation: String) -> Self {
        Self {
            schema,
            relation,
            filter: String::new(),
            applied: String::new(),
            order: None,
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
    pub selected_cell: Option<(usize, usize)>,
    pub kind: TabKind,
    /// Set while the shown result is read a page at a time.
    pub paging: Option<Paging>,
    /// The last run, for refreshing it.
    pub last_run: Option<(Executed, ExecOptions)>,
    pub highlighter: Highlighter,
    pub cells: CellCache,
}

impl QueryTab {
    pub fn data(&self) -> Option<&DataView> {
        match &self.kind {
            TabKind::Data(view) => Some(view),
            TabKind::Script => None,
        }
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
        let within = executed
            .sql
            .char_indices()
            .nth(position.saturating_sub(1))
            .map_or(executed.sql.len(), |(at, _)| at);
        Some(executed.offset + within)
    }
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
            selected_cell: None,
            kind,
            paging: None,
            last_run: None,
            highlighter: Highlighter::default(),
            cells: CellCache::default(),
        });
        self.active = Some(id);
        id
    }

    /// A title for a new editor: `Script 3`.
    pub fn next_script_title(&self) -> String {
        let used = self
            .tabs
            .iter()
            .filter(|tab| matches!(tab.kind, TabKind::Script))
            .count();
        format!("Script {}", used + 1)
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
            },
        });
        assert_eq!(tab.error_offset(), Some(10));
    }
}
