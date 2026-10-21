//! Changing rows in the grid, and asking before statements that change
//! every row of a table.
//!
//! Rows can be edited when they map back to one table with a primary key:
//! a data tab, or a `SELECT` of one table's plain columns. Cells can be
//! changed, rows added and rows deleted; the changes stay in the grid until
//! the user reviews the `DELETE`s, `UPDATE`s and `INSERT`s they make and
//! applies them. They run as one transaction, each row updated or deleted
//! picked by its primary key. Key columns are only typed in added rows.

use std::collections::{BTreeMap, BTreeSet};
use std::time::SystemTime;

use egui::{CornerRadius, RichText, Stroke};
use violet_app::ExecOptions;
use violet_core::ports::HistoryEntry;
use violet_core::{ColumnInfo, ConnectionId, Dialect as _, ObjectKind, QueryOutput, Result};

use super::action::{Action, RunScope};
use super::definition_flow::Family;
use super::icons::Icon;
use super::selection::Mode;
use super::state::{Link, Loadable};
use super::theme::{self, Palette};
use super::toasts::Toast;
use super::widgets::{self, Tone};
use super::workspace::{InlineCell, Outcome, TabId};
use super::{VioletApp, connection_name};
use crate::sql::{self, TableRef};

/// Whether an editor's rows can be changed.
pub enum Editing {
    /// Not editable; the reason is shown when there is one.
    Off(String),
    /// The table's columns are being read.
    Loading,
    On(EditTarget),
}

impl Editing {
    fn off(reason: &str) -> Self {
        Self::Off(reason.to_owned())
    }
}

/// The table rows are written back to.
pub struct EditTarget {
    pub schema: String,
    pub relation: String,
    /// The result columns of the primary key.
    pub keys: Vec<usize>,
    /// Per result column: whether it can be changed.
    pub editable: Vec<bool>,
    /// Per result column: whether an added row can set it, keys included.
    pub insertable: Vec<bool>,
}

/// A cell being edited.
pub struct CellEditor {
    pub tab: TabId,
    pub row: usize,
    pub column: usize,
    pub name: String,
    pub text: String,
    pub null: bool,
}

/// Changes waiting for the user to apply them.
pub struct EditReview {
    pub tab: TabId,
    pub table: String,
    pub sql: String,
    /// The rows changed, as "3 rows (1 updated, 2 inserted)".
    pub summary: String,
}

/// A run waiting for the user to accept statements without `WHERE`.
pub struct RunReview {
    pub tab: TabId,
    pub scope: RunScope,
    pub writes: Vec<String>,
}

/// Changes being written.
pub struct Applying {
    pub tab: TabId,
    pub connection: ConnectionId,
    pub table: String,
    pub sql: String,
    pub summary: String,
}

impl VioletApp {
    /// Whether the rows `tab_id` shows can be edited. Reads the catalog it
    /// needs to tell, the first time.
    pub(super) fn editing(&mut self, tab_id: TabId) -> Editing {
        let Some(tab) = self.workspace.get(tab_id) else {
            return Editing::off("");
        };
        let Some(Outcome::Done { output }) = &tab.outcome else {
            return Editing::off("");
        };
        let [set] = output.sets.as_slice() else {
            return Editing::off("");
        };
        if !set.has_rows() || tab.running.is_some() {
            return Editing::off("");
        }
        if self
            .applying
            .values()
            .any(|applying| applying.tab == tab_id)
        {
            return Editing::off("Saving changes…");
        }
        let Some(connection) = tab.connection else {
            return Editing::off("");
        };
        let Some(source) = tab.source.clone() else {
            return Editing::off(
                "Only rows of one table can be edited: a data tab, or a SELECT of one \
                 table's columns, without joins, grouping or computed columns.",
            );
        };
        if !self.explorer.link(connection).is_online() {
            return Editing::off("Not connected");
        }
        let names: Vec<String> = set.columns.iter().map(|c| c.name.clone()).collect();

        let (schema, relation) = match self.find_table(connection, &source) {
            Found::Table(schema, relation) => (schema, relation),
            Found::Loading => return Editing::Loading,
            Found::NotTable => return Editing::off("Only tables can be edited, not views."),
            Found::Missing => return Editing::off("The table was not found in the catalog."),
        };
        let key = (connection, schema.clone(), relation.clone());
        let columns: std::sync::Arc<[ColumnInfo]> = match self.explorer.columns.get(&key) {
            Some(Loadable::Ready(columns)) => columns.clone(),
            Some(Loadable::Loading) => return Editing::Loading,
            Some(Loadable::Failed(error)) => return Editing::Off(error.clone()),
            Some(Loadable::Idle) | None => {
                self.load_columns(connection, schema, relation);
                return Editing::Loading;
            }
        };
        target(schema, relation, &columns, &names)
    }

    /// The schema and name the catalog has for `source`.
    pub(super) fn find_table(&mut self, connection: ConnectionId, source: &TableRef) -> Found {
        let schemas: Vec<String> = match &source.schema {
            Some(schema) => match self.explorer.schemas.get(&connection) {
                Some(Loadable::Ready(all)) => all
                    .iter()
                    .filter(|name| schema.matches(name))
                    .cloned()
                    .collect(),
                Some(Loadable::Loading | Loadable::Idle) | None => return Found::Loading,
                Some(Loadable::Failed(_)) => return Found::Missing,
            },
            None => self.explorer.search_path(connection),
        };
        let mut loading = false;
        for schema in schemas {
            match self.explorer.objects.get(&(connection, schema.clone())) {
                Some(Loadable::Ready(objects)) => {
                    let found = objects
                        .iter()
                        .filter(|object| object.kind.is_relation())
                        .find(|object| source.name.matches(&object.name));
                    if let Some(object) = found {
                        // An earlier schema still loading could hold the
                        // same name first.
                        if loading {
                            return Found::Loading;
                        }
                        return if object.kind == ObjectKind::Table {
                            Found::Table(schema, object.name.clone())
                        } else {
                            Found::NotTable
                        };
                    }
                }
                Some(Loadable::Loading) => loading = true,
                Some(Loadable::Failed(_)) => {}
                Some(Loadable::Idle) | None => {
                    self.load_objects(connection, schema);
                    loading = true;
                }
            }
        }
        if loading {
            Found::Loading
        } else {
            Found::Missing
        }
    }

    pub(super) fn edit_action(&mut self, ctx: &egui::Context, action: Action) {
        match action {
            Action::EditCell(tab_id, row, column) => {
                let Some(tab) = self.workspace.get(tab_id) else {
                    return;
                };
                let Some(Outcome::Done { output }) = &tab.outcome else {
                    return;
                };
                let Some(set) = output.sets.first() else {
                    return;
                };
                let value = match tab.edits.get(&(row, column)) {
                    Some(value) => value.clone(),
                    // An added row's cell starts empty, not NULL.
                    None if row >= set.rows.len() => Some(String::new()),
                    None => set.rows.get(row, column).flatten().map(str::to_owned),
                };
                self.cell_editor = Some(CellEditor {
                    tab: tab_id,
                    row,
                    column,
                    name: set
                        .columns
                        .get(column)
                        .map(|c| c.name.clone())
                        .unwrap_or_default(),
                    null: value.is_none(),
                    text: value.unwrap_or_default(),
                });
            }
            Action::SetCell(tab_id, row, column, value) => {
                let Some(tab) = self.workspace.get_mut(tab_id) else {
                    return;
                };
                let original = match &tab.outcome {
                    Some(Outcome::Done { output }) => output
                        .sets
                        .first()
                        .and_then(|set| set.rows.get(row, column))
                        .map(|value| value.map(str::to_owned)),
                    _ => None,
                };
                // Back to what the server has: no change.
                if original.as_ref() == Some(&value) {
                    tab.edits.remove(&(row, column));
                } else {
                    tab.edits.insert((row, column), value);
                }
            }
            Action::RevertCell(tab_id, row, column) => {
                if let Some(tab) = self.workspace.get_mut(tab_id) {
                    tab.edits.remove(&(row, column));
                }
            }
            Action::DiscardEdits(tab_id) => {
                if let Some(tab) = self.workspace.get_mut(tab_id) {
                    tab.clear_changes();
                    tab.grid_selection.clear();
                }
            }
            Action::AddRow(tab_id) => {
                // The new row opens on its first column with no default
                // to keep: past the key, when there is one to pass.
                let column = match self.editing(tab_id) {
                    Editing::On(target) => target
                        .editable
                        .iter()
                        .position(|on| *on)
                        .or_else(|| target.insertable.iter().position(|on| *on)),
                    Editing::Off(_) | Editing::Loading => None,
                };
                let Some(tab) = self.workspace.get_mut(tab_id) else {
                    return;
                };
                let Some(Outcome::Done { output }) = &tab.outcome else {
                    return;
                };
                let Some(set) = output.sets.first() else {
                    return;
                };
                let row = set.rows.len() + tab.added;
                tab.added += 1;
                tab.reveal_row = Some(row);
                tab.grid_selection
                    .click((row, column.unwrap_or(0)), Mode::Replace);
                tab.inline = column.map(|column| InlineCell::new(row, column, ""));
            }
            Action::DeleteRows(tab_id, mut rows) => {
                let Some(tab) = self.workspace.get_mut(tab_id) else {
                    return;
                };
                let Some(Outcome::Done { output }) = &tab.outcome else {
                    return;
                };
                let Some(set) = output.sets.first() else {
                    return;
                };
                let server_rows = set.rows.len();
                // Last first: removing an added row moves the ones after it.
                rows.sort_unstable_by(|a, b| b.cmp(a));
                rows.dedup();
                for row in rows {
                    if row < server_rows {
                        tab.deleted.insert(row);
                    } else if row < server_rows + tab.added {
                        let after = tab.edits.split_off(&(row, 0));
                        for ((at, column), value) in after {
                            if at > row {
                                tab.edits.insert((at - 1, column), value);
                            }
                        }
                        tab.added -= 1;
                    }
                }
                tab.grid_selection.clear();
                tab.inline = None;
            }
            Action::RestoreRows(tab_id, rows) => {
                if let Some(tab) = self.workspace.get_mut(tab_id) {
                    for row in rows {
                        tab.deleted.remove(&row);
                    }
                }
            }
            Action::ReviewEdits(tab_id) => match self.edit_statements(tab_id) {
                Ok(review) => self.edit_review = Some(review),
                Err(error) => self.toasts.push(
                    ctx,
                    Toast::error("These changes cannot be saved", error.message),
                ),
            },
            Action::ApplyEdits(tab_id) => {
                let Some(review) = self.edit_review.take().filter(|r| r.tab == tab_id) else {
                    return;
                };
                let Some(connection) = self.workspace.get(tab_id).and_then(|tab| tab.connection)
                else {
                    return;
                };
                // One execute: the server runs the statements as one
                // transaction, all or nothing.
                let request =
                    self.backend
                        .execute(connection, review.sql.clone(), ExecOptions::WRITE);
                self.applying.insert(
                    request,
                    Applying {
                        tab: tab_id,
                        connection,
                        table: review.table,
                        sql: review.sql,
                        summary: review.summary,
                    },
                );
            }
            Action::RunConfirmed(tab_id, scope) => self.run_checked(ctx, tab_id, scope, true),
            _ => {}
        }
    }

    /// The statements that write a tab's changes: `DELETE`s and `UPDATE`s
    /// picking each row by its primary key, then `INSERT`s.
    fn edit_statements(&mut self, tab_id: TabId) -> Result<EditReview> {
        let refuse =
            |message: &str| violet_core::DbError::new(violet_core::ErrorKind::Other, message);
        let Editing::On(target) = self.editing(tab_id) else {
            return Err(refuse("The rows cannot be edited any more."));
        };
        let tab = self
            .workspace
            .get(tab_id)
            .ok_or_else(|| refuse("The editor is gone."))?;
        let Some(Outcome::Done { output }) = &tab.outcome else {
            return Err(refuse("The rows are gone."));
        };
        let set = output
            .sets
            .first()
            .ok_or_else(|| refuse("The rows are gone."))?;
        let profile = tab
            .connection
            .and_then(|id| super::session_profile(&self.profiles, &self.explorer, id))
            .ok_or_else(|| refuse("The connection is gone."))?;
        let driver = self
            .backend
            .registry()
            .get(&profile.driver)
            .ok_or_else(|| refuse("The database's provider is not loaded."))?;
        let dialect = driver.info().dialect;
        let changes = Changes {
            edits: &tab.edits,
            added: tab.added,
            deleted: &tab.deleted,
        };
        let plan = plan(
            &target,
            set,
            &changes,
            &|name| dialect.quote_ident(name),
            Family::of(profile.driver.as_str()) == Family::MySql,
        )?;
        Ok(EditReview {
            tab: tab_id,
            table: format!("{}.{}", target.schema, target.relation),
            summary: plan.summary(),
            sql: plan.statements.join("\n\n"),
        })
    }

    /// The changes ran, or failed and changed nothing.
    pub(super) fn edits_applied(
        &mut self,
        ctx: &egui::Context,
        applying: Applying,
        result: &Result<QueryOutput>,
    ) {
        let at = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |since| {
                u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
            });
        let source = connection_name(&self.profiles, &self.explorer, applying.connection)
            .unwrap_or_default();
        match result {
            Ok(output) => {
                let updated: u64 = output.sets.iter().filter_map(|set| set.affected).sum();
                let missed = output
                    .sets
                    .iter()
                    .filter(|set| set.affected == Some(0))
                    .count();
                self.record(HistoryEntry {
                    at,
                    connection: Some(applying.connection),
                    source,
                    sql: sql::redact_secrets(&applying.sql),
                    elapsed_ms: u64::try_from(output.elapsed.as_millis()).unwrap_or(u64::MAX),
                    rows: Some(updated),
                    error: None,
                });
                if let Some(tab) = self.workspace.get_mut(applying.tab) {
                    tab.clear_changes();
                    tab.grid_selection.clear();
                }
                if missed == 0 {
                    self.toasts.push(
                        ctx,
                        Toast::success(format!("Saved {}: {}", applying.table, applying.summary)),
                    );
                } else {
                    self.toasts.push(
                        ctx,
                        Toast::error(
                            format!(
                                "{missed} {} not found",
                                if missed == 1 { "row was" } else { "rows were" }
                            ),
                            "They were changed or deleted since they were read. The other \
                             changes were saved; the rows are read again.",
                        ),
                    );
                }
                self.refresh(ctx, applying.tab);
            }
            Err(error) => {
                self.record(HistoryEntry {
                    at,
                    connection: Some(applying.connection),
                    source,
                    sql: sql::redact_secrets(&applying.sql),
                    elapsed_ms: 0,
                    rows: None,
                    error: Some(error.message.clone()),
                });
                self.toasts.push(
                    ctx,
                    Toast::error(
                        "The changes were not saved",
                        format!(
                            "{} Nothing was changed; your edits are kept.",
                            error.message
                        ),
                    ),
                );
            }
        }
    }

    /// Statements in what `run` would send that change every row of a
    /// table, when the user has not accepted them yet.
    pub(super) fn unbounded_writes(&self, text: &str, connection: ConnectionId) -> Vec<String> {
        if !matches!(self.explorer.link(connection), Link::Online(_)) {
            return Vec::new();
        }
        sql::unbounded_writes(text)
    }
}

pub(super) enum Found {
    Table(String, String),
    NotTable,
    Missing,
    Loading,
}

/// Which result columns are the key and which can change: columns of the
/// table, named once in the result, outside the key.
fn target(schema: String, relation: String, columns: &[ColumnInfo], names: &[String]) -> Editing {
    let keys: Vec<&ColumnInfo> = columns.iter().filter(|c| c.primary_key).collect();
    if keys.is_empty() {
        return Editing::off(
            "The table has no primary key, so its rows cannot be told apart safely.",
        );
    }
    let once = |name: &str| names.iter().filter(|n| *n == name).count() == 1;
    let mut key_columns = Vec::new();
    for key in keys {
        match names.iter().position(|name| *name == key.name) {
            Some(index) if once(&key.name) => key_columns.push(index),
            _ => {
                return Editing::Off(format!(
                    "Add the primary key column {} to the query to edit its rows.",
                    key.name
                ));
            }
        }
    }
    let insertable: Vec<bool> = names
        .iter()
        .map(|name| once(name) && columns.iter().any(|column| column.name == *name))
        .collect();
    let editable = insertable
        .iter()
        .enumerate()
        .map(|(index, insertable)| *insertable && !key_columns.contains(&index))
        .collect();
    Editing::On(EditTarget {
        schema,
        relation,
        keys: key_columns,
        editable,
        insertable,
    })
}

/// A grid's changes not written yet.
struct Changes<'a> {
    edits: &'a BTreeMap<(usize, usize), Option<String>>,
    added: usize,
    deleted: &'a BTreeSet<usize>,
}

/// The statements that write a grid's changes, and how many rows each
/// kind touches.
struct Plan {
    statements: Vec<String>,
    updated: usize,
    inserted: usize,
    deleted: usize,
}

impl Plan {
    /// "3 rows (1 updated, 2 inserted)".
    fn summary(&self) -> String {
        let parts: Vec<String> = [
            (self.updated, "updated"),
            (self.inserted, "inserted"),
            (self.deleted, "deleted"),
        ]
        .into_iter()
        .filter(|(count, _)| *count > 0)
        .map(|(count, verb)| format!("{count} {verb}"))
        .collect();
        let total = self.updated + self.inserted + self.deleted;
        format!(
            "{total} {} ({})",
            if total == 1 { "row" } else { "rows" },
            parts.join(", ")
        )
    }
}

/// Deletes first, then updates of the rows kept, then inserts. A row
/// without values is inserted with its defaults alone, which MySQL
/// writes `() VALUES ()`.
fn plan(
    target: &EditTarget,
    set: &violet_core::ResultSet,
    changes: &Changes<'_>,
    quote: &dyn Fn(&str) -> String,
    mysql: bool,
) -> Result<Plan> {
    let refuse = |message: &str| violet_core::DbError::new(violet_core::ErrorKind::Other, message);
    let column_name = |column: usize| {
        set.columns
            .get(column)
            .map_or_else(String::new, |c| c.name.clone())
    };
    let value = |column: usize, value: &Option<String>| match value {
        Some(text) => literal(text).ok_or_else(|| {
            refuse(&format!(
                "The value of {} has a backslash or a control character, which the grid does \
                 not write. Write it with a statement in the SQL editor.",
                column_name(column)
            ))
        }),
        None => Ok("NULL".to_owned()),
    };
    let picked = |row: usize| -> Result<String> {
        let mut keys = Vec::new();
        for key in &target.keys {
            let value = set
                .rows
                .get(row, *key)
                .flatten()
                .ok_or_else(|| refuse("A row has no primary key value."))?;
            let value = literal(value).ok_or_else(|| {
                refuse("A row's primary key has characters the grid does not write.")
            })?;
            keys.push(format!("{} = {value}", quote(&column_name(*key))));
        }
        Ok(keys.join(" AND "))
    };
    let table = format!("{}.{}", quote(&target.schema), quote(&target.relation));
    let server_rows = set.rows.len();
    let mut plan = Plan {
        statements: Vec::new(),
        updated: 0,
        inserted: 0,
        deleted: 0,
    };

    for row in changes.deleted.iter().filter(|row| **row < server_rows) {
        plan.statements
            .push(format!("DELETE FROM {table}\nWHERE {};", picked(*row)?));
        plan.deleted += 1;
    }

    let mut rows: BTreeMap<usize, Vec<(usize, &Option<String>)>> = BTreeMap::new();
    for ((row, column), change) in changes.edits {
        if *row >= server_rows || changes.deleted.contains(row) {
            continue;
        }
        if !target.editable.get(*column).copied().unwrap_or(false) {
            return Err(refuse(&format!(
                "{} cannot be changed here.",
                column_name(*column)
            )));
        }
        rows.entry(*row).or_default().push((*column, change));
    }
    for (row, cells) in &rows {
        let mut sets = Vec::new();
        for (column, change) in cells {
            sets.push(format!(
                "{} = {}",
                quote(&column_name(*column)),
                value(*column, change)?
            ));
        }
        plan.statements.push(format!(
            "UPDATE {table}\nSET {}\nWHERE {};",
            sets.join(",\n    "),
            picked(*row)?
        ));
        plan.updated += 1;
    }

    for row in server_rows..server_rows + changes.added {
        let mut names = Vec::new();
        let mut values = Vec::new();
        for ((_, column), change) in changes.edits.range((row, 0)..(row + 1, 0)) {
            if !target.insertable.get(*column).copied().unwrap_or(false) {
                return Err(refuse(&format!(
                    "{} is not a column of the table.",
                    column_name(*column)
                )));
            }
            names.push(quote(&column_name(*column)));
            values.push(value(*column, change)?);
        }
        plan.statements.push(if names.is_empty() && mysql {
            format!("INSERT INTO {table} () VALUES ();")
        } else if names.is_empty() {
            format!("INSERT INTO {table} DEFAULT VALUES;")
        } else {
            format!(
                "INSERT INTO {table} ({})\nVALUES ({});",
                names.join(", "),
                values.join(", ")
            )
        });
        plan.inserted += 1;
    }

    if plan.statements.is_empty() {
        return Err(refuse("Nothing changed."));
    }
    Ok(plan)
}

/// `text` as a string literal the grid may send: quotes doubled, and
/// nothing a database might read as an escape (a backslash, NUL or a
/// control character other than line breaks and tabs).
fn literal(text: &str) -> Option<String> {
    let safe = text
        .chars()
        .all(|c| c != '\\' && (!c.is_control() || matches!(c, '\n' | '\r' | '\t')));
    safe.then(|| format!("'{}'", text.replace('\'', "''")))
}

/// Edits a cell's value. Returns whether it is still open.
pub fn show_cell_editor(
    ctx: &egui::Context,
    editor: &mut CellEditor,
    actions: &mut Vec<Action>,
) -> bool {
    let palette = Palette::current(ctx);
    let mut open = true;
    let modal = egui::Modal::new(egui::Id::new("cell-editor"))
        .frame(frame(&palette))
        .show(ctx, |ui| {
            ui.set_width(460.0);
            ui.spacing_mut().item_spacing.y = 12.0;
            ui.label(
                RichText::new(format!("Edit {}", editor.name))
                    .font(theme::semibold(16.0))
                    .color(palette.text),
            );
            ui.add_enabled_ui(!editor.null, |ui| {
                let response = ui.add(
                    egui::TextEdit::multiline(&mut editor.text)
                        .font(theme::mono(12.5))
                        .desired_rows(6)
                        .desired_width(f32::INFINITY),
                );
                if !editor.null && !response.has_focus() && !response.lost_focus() {
                    response.request_focus();
                }
            });
            widgets::checkbox(ui, &mut editor.null, "NULL");
            ui.label(
                RichText::new("The change stays in the grid until you save the table's changes.")
                    .color(palette.dim),
            );
            let apply_key =
                ui.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::Enter));
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let tip = format!(
                        "Apply ({})",
                        ui.ctx().format_shortcut(&egui::KeyboardShortcut::new(
                            egui::Modifiers::COMMAND,
                            egui::Key::Enter
                        ))
                    );
                    if widgets::button(ui, Tone::Primary, Some(Icon::Check), "Apply")
                        .on_hover_text(tip)
                        .clicked()
                        || apply_key
                    {
                        let value = (!editor.null).then(|| editor.text.clone());
                        actions.push(Action::SetCell(
                            editor.tab,
                            editor.row,
                            editor.column,
                            value,
                        ));
                        open = false;
                    }
                    if widgets::button(ui, Tone::Ghost, None, "Cancel").clicked() {
                        open = false;
                    }
                });
            });
        });
    open && !modal.should_close()
}

/// Shows the statements that save a table's changes. Returns whether it is
/// still open.
pub fn show_edit_review(
    ctx: &egui::Context,
    review: &EditReview,
    actions: &mut Vec<Action>,
) -> bool {
    let palette = Palette::current(ctx);
    let mut open = true;
    let modal = egui::Modal::new(egui::Id::new("edit-review"))
        .frame(frame(&palette))
        .show(ctx, |ui| {
            ui.set_width(560.0);
            ui.spacing_mut().item_spacing.y = 12.0;
            ui.label(
                RichText::new(format!("Save changes to {}?", review.table))
                    .font(theme::semibold(16.0))
                    .color(palette.text),
            );
            ui.add(
                egui::Label::new(
                    RichText::new(format!(
                        "{} change in one transaction: if any statement fails, nothing \
                         changes. Rows updated or deleted are each picked by its primary key. \
                         Review what will run:",
                        review.summary
                    ))
                    .color(palette.secondary),
                )
                .wrap(),
            );
            egui::Frame::new()
                .fill(palette.window)
                .stroke(Stroke::new(1.0, palette.outline))
                .corner_radius(CornerRadius::same(8))
                .inner_margin(egui::Margin::same(10))
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .max_height(280.0)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            ui.add(
                                egui::Label::new(
                                    RichText::new(&review.sql)
                                        .font(theme::mono(12.0))
                                        .color(palette.text),
                                )
                                .selectable(true),
                            );
                        });
                });
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if widgets::button(ui, Tone::Danger, Some(Icon::Save), "Apply changes")
                        .clicked()
                    {
                        // The action takes the review: closing it here
                        // would leave it nothing to apply.
                        actions.push(Action::ApplyEdits(review.tab));
                    }
                    if widgets::button(ui, Tone::Ghost, None, "Cancel").clicked() {
                        open = false;
                    }
                });
            });
        });
    open && !modal.should_close()
}

/// Asks before running statements that change every row of a table.
/// Returns whether it is still open.
pub fn show_run_review(ctx: &egui::Context, review: &RunReview, actions: &mut Vec<Action>) -> bool {
    let palette = Palette::current(ctx);
    let mut open = true;
    let modal = egui::Modal::new(egui::Id::new("run-review"))
        .frame(frame(&palette))
        .show(ctx, |ui| {
            ui.set_width(440.0);
            ui.spacing_mut().item_spacing.y = 12.0;
            ui.horizontal(|ui| {
                let (rect, _) =
                    ui.allocate_exact_size(egui::Vec2::splat(18.0), egui::Sense::hover());
                widgets::paint_icon(ui, Icon::ShieldAlert, rect, palette.danger);
                ui.label(
                    RichText::new("Change every row?")
                        .font(theme::semibold(16.0))
                        .color(palette.text),
                );
            });
            ui.add(
                egui::Label::new(
                    RichText::new(
                        "These statements have no WHERE, so they change or delete every row \
                         of their table:",
                    )
                    .color(palette.secondary),
                )
                .wrap(),
            );
            for write in &review.writes {
                ui.label(
                    RichText::new(format!("•  {write}"))
                        .font(theme::mono(12.5))
                        .color(palette.danger),
                );
            }
            ui.label(RichText::new("Run them anyway?").color(palette.secondary));
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if widgets::button(ui, Tone::Danger, Some(Icon::Play), "Run anyway").clicked() {
                        actions.push(Action::RunConfirmed(review.tab, review.scope));
                        open = false;
                    }
                    if widgets::button(ui, Tone::Ghost, None, "Cancel").clicked() {
                        open = false;
                    }
                });
            });
        });
    open && !modal.should_close()
}

pub(super) fn frame(palette: &Palette) -> egui::Frame {
    egui::Frame::new()
        .fill(palette.surface)
        .stroke(Stroke::new(1.0, palette.outline))
        .corner_radius(CornerRadius::same(14))
        .inner_margin(egui::Margin::same(22))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn column(name: &str, primary_key: bool) -> ColumnInfo {
        ColumnInfo {
            name: name.to_owned(),
            data_type: "text".to_owned(),
            nullable: true,
            default: None,
            primary_key,
            comment: None,
        }
    }

    #[test]
    fn only_safe_literals_are_written() {
        assert_eq!(literal("O'Brien"), Some("'O''Brien'".to_owned()));
        assert_eq!(
            literal("two\nlines\tok"),
            Some("'two\nlines\tok'".to_owned())
        );
        assert_eq!(literal("C:\\temp"), None);
        assert_eq!(literal("nul\0"), None);
    }

    #[test]
    fn keys_must_be_in_the_result_and_are_not_editable() {
        let columns = [column("id", true), column("nome", false)];
        let names = |list: &[&str]| list.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>();
        let Editing::On(target) = target(
            "public".into(),
            "t".into(),
            &columns,
            &names(&["id", "nome", "extra"]),
        ) else {
            panic!("editable");
        };
        assert_eq!(target.keys, [0]);
        assert_eq!(target.editable, [false, true, false]);
        // Added rows set the key too.
        assert_eq!(target.insertable, [true, true, false]);

        assert!(matches!(
            target_of(&columns, &names(&["nome"])),
            Editing::Off(_)
        ));
        assert!(matches!(
            target_of(&[column("nome", false)], &names(&["nome"])),
            Editing::Off(_)
        ));
        // A column named twice could be either.
        let Editing::On(twice) = target_of(&columns, &names(&["id", "nome", "nome"])) else {
            panic!("editable");
        };
        assert_eq!(twice.editable, [false, false, false]);
    }

    /// Rows `(1, 'a')`, `(2, 'b')`, `(3, 'c')` of `t (id, nome)`.
    fn rows() -> (EditTarget, violet_core::ResultSet) {
        let Editing::On(target) = target_of(
            &[column("id", true), column("nome", false)],
            &["id".to_owned(), "nome".to_owned()],
        ) else {
            panic!("editable");
        };
        let meta = |name: &str| violet_core::ColumnMeta {
            name: name.into(),
            type_name: None,
            numeric: false,
        };
        let mut set = violet_core::ResultSet {
            columns: vec![meta("id"), meta("nome")],
            ..violet_core::ResultSet::default()
        };
        set.rows.push([Some("1"), Some("a")]);
        set.rows.push([Some("2"), Some("b")]);
        set.rows.push([Some("3"), Some("c")]);
        (target, set)
    }

    fn quote(name: &str) -> String {
        format!("\"{name}\"")
    }

    #[test]
    fn deletes_then_updates_then_inserts() {
        let (target, set) = rows();
        let mut edits = BTreeMap::new();
        edits.insert((0, 1), Some("x".to_owned()));
        // A deleted row's change is not written.
        edits.insert((1, 1), Some("y".to_owned()));
        edits.insert((3, 0), Some("9".to_owned()));
        edits.insert((3, 1), None);
        let deleted = BTreeSet::from([1]);
        let changes = Changes {
            edits: &edits,
            added: 2,
            deleted: &deleted,
        };
        let plan = plan(&target, &set, &changes, &quote, false).expect("plan");
        assert_eq!(
            plan.statements,
            [
                "DELETE FROM \"public\".\"t\"\nWHERE \"id\" = '2';",
                "UPDATE \"public\".\"t\"\nSET \"nome\" = 'x'\nWHERE \"id\" = '1';",
                "INSERT INTO \"public\".\"t\" (\"id\", \"nome\")\nVALUES ('9', NULL);",
                "INSERT INTO \"public\".\"t\" DEFAULT VALUES;",
            ]
        );
        assert_eq!(plan.summary(), "4 rows (1 updated, 2 inserted, 1 deleted)");
        let mysql = plan_of(&target, &set, &changes, true);
        assert_eq!(
            mysql.last().map(String::as_str),
            Some("INSERT INTO \"public\".\"t\" () VALUES ();")
        );
    }

    #[test]
    fn nothing_to_write_is_refused() {
        let (target, set) = rows();
        let edits = BTreeMap::new();
        let deleted = BTreeSet::new();
        let changes = Changes {
            edits: &edits,
            added: 0,
            deleted: &deleted,
        };
        assert!(plan(&target, &set, &changes, &quote, false).is_err());
    }

    fn plan_of(
        target: &EditTarget,
        set: &violet_core::ResultSet,
        changes: &Changes<'_>,
        mysql: bool,
    ) -> Vec<String> {
        plan(target, set, changes, &quote, mysql)
            .expect("plan")
            .statements
    }

    fn target_of(columns: &[ColumnInfo], names: &[String]) -> Editing {
        target("public".into(), "t".into(), columns, names)
    }
}
