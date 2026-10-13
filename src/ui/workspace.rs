//! Open tabs: SQL editors and table data views.

use std::ops::Range;
use std::time::{Duration, Instant};

use violet_app::RequestId;
use violet_core::{ConnectionId, DbError, QueryOutput};

use super::editor::Highlighter;

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
    /// Opened from the tree to browse a relation: the editor starts small.
    pub data_view: bool,
    pub highlighter: Highlighter,
}

impl QueryTab {
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
        data_view: bool,
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
            data_view,
            highlighter: Highlighter::default(),
        });
        self.active = Some(id);
        id
    }

    /// A title for a new editor: `Script 3`.
    pub fn next_script_title(&self) -> String {
        let used = self.tabs.iter().filter(|tab| !tab.data_view).count();
        format!("Script {}", used + 1)
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
        let a = workspace.open("A".into(), None, String::new(), false);
        let b = workspace.open("B".into(), None, String::new(), false);
        let c = workspace.open("C".into(), None, String::new(), false);
        workspace.active = Some(b);
        workspace.close(b);
        assert_eq!(workspace.active, Some(c));
        workspace.close(c);
        assert_eq!(workspace.active, Some(a));
        workspace.close(a);
        assert_eq!(workspace.active, None);
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
        let id = workspace.open("A".into(), None, "SELECT 1;\nSELEC 2".into(), false);
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
