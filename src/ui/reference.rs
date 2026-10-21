//! The rows foreign keys point at, in an extra row of the grid.
//!
//! A foreign key's values show as links. Following one reads the rows of
//! the referenced table it points at and shows them in a row opened under
//! the one clicked, as a small grid whose foreign keys link on in turn:
//! each key followed is a step, and the breadcrumb goes back to any of
//! them. It works where the rows map back to one table (as for editing),
//! whose foreign keys the catalog lists.

use egui::{Align, CornerRadius, Layout, Rect, RichText, Sense, Stroke, Ui, UiBuilder, Vec2};
use violet_app::{CachePolicy, ExecOptions, RequestId};
use violet_core::{ConnectionId, Dialect, ForeignKey, Page, QueryOutput, Result, ResultSet};

use super::VioletApp;
use super::action::Action;
use super::cells::{self, Cell, Look};
use super::edit_flow::Found;
use super::icons::Icon;
use super::state::Loadable;
use super::theme::{self, Palette};
use super::widgets;
use super::workspace::{Outcome, TabId};

/// Referenced rows read per step: a key usually points at one.
const ROWS: u64 = 50;

/// Referenced rows shown before the rest are left to "Open table".
const SHOWN: usize = 5;

const ROW_HEIGHT: f32 = 24.0;
/// The line with the breadcrumb and the buttons.
const TITLE_HEIGHT: f32 = 30.0;
/// Room around the small grid, and for its scroll bar.
const MARGIN: f32 = 8.0;

/// The keys followed from a row of a tab's grid.
pub struct Reference {
    pub tab: TabId,
    pub connection: ConnectionId,
    /// The row of the tab's result the extra row opens under.
    pub row: usize,
    /// That row's values when it opened: when they change (a new run, a
    /// change saved), the extra row closes.
    cells: Vec<Option<String>>,
    /// Each key followed, in order; the last one shows.
    pub steps: Vec<Step>,
}

/// One key followed: the rows it points at.
pub struct Step {
    pub key: ForeignKey,
    /// The values of the key's columns, in key order.
    pub values: Vec<String>,
    pub lookup: Lookup,
    /// Per column of the rows found: whether it is part of one of the
    /// referenced table's own foreign keys. Empty until they are known.
    links: Vec<bool>,
}

pub enum Lookup {
    Loading(RequestId),
    Found(ResultSet),
    Failed(String),
}

/// What the grid needs to know about the tab's foreign keys.
#[derive(Default)]
pub struct Links {
    /// Per result column: whether it is part of a foreign key.
    pub columns: Vec<bool>,
}

/// Rows whose foreign keys can be followed: their columns, and one row.
struct Source<'a> {
    keys: &'a [ForeignKey],
    names: &'a [String],
    numeric: &'a [bool],
    row: &'a [Option<String>],
}

/// Per column in `names`: whether one of `keys` takes it.
fn link_columns(keys: &[ForeignKey], names: &[String]) -> Vec<bool> {
    names
        .iter()
        .map(|name| keys.iter().any(|key| key.columns.contains(name)))
        .collect()
}

impl VioletApp {
    /// Which columns of `tab_id`'s rows are foreign keys, loading what is
    /// missing; and the same for the rows its extra row shows. Closes the
    /// extra row when the row it opened under changed.
    pub(super) fn sync_reference(&mut self, tab_id: TabId) -> Links {
        self.sync_nested_links(tab_id);
        let target = self.reference_target(tab_id);
        if let Some(reference) = self.reference.as_ref().filter(|r| r.tab == tab_id) {
            let now = target
                .as_ref()
                .and_then(|_| self.result_row(tab_id, reference.row));
            if now.as_ref() != Some(&reference.cells) {
                self.reference = None;
            }
        }
        match target {
            Some(target) => Links {
                columns: link_columns(&target.keys, &target.names),
            },
            None => Links::default(),
        }
    }

    /// The values of `row` of `tab_id`'s result.
    fn result_row(&self, tab_id: TabId, row: usize) -> Option<Vec<Option<String>>> {
        let tab = self.workspace.get(tab_id)?;
        let Some(Outcome::Done { output }) = &tab.outcome else {
            return None;
        };
        let cells = output.sets.get(tab.active_set)?.rows.row(row)?;
        Some(cells.iter().map(|v| v.map(str::to_owned)).collect())
    }

    /// Finds the foreign keys of the rows the extra row shows, once.
    fn sync_nested_links(&mut self, tab_id: TabId) {
        let Some(reference) = self.reference.as_ref().filter(|r| r.tab == tab_id) else {
            return;
        };
        let Some(step) = reference.steps.last() else {
            return;
        };
        let Lookup::Found(set) = &step.lookup else {
            return;
        };
        if !step.links.is_empty() {
            return;
        }
        let connection = reference.connection;
        let (schema, table) = (step.key.ref_schema.clone(), step.key.ref_table.clone());
        let names: Vec<String> = set.columns.iter().map(|c| c.name.clone()).collect();
        let Some(keys) = self.keys_from(connection, &schema, &table, &names) else {
            return;
        };
        if let Some(step) = self.reference.as_mut().and_then(|r| r.steps.last_mut()) {
            step.links = link_columns(&keys, &names);
        }
    }

    /// The foreign keys from `schema.table` whose columns are all among
    /// `names`; `None` while the schema's are loading, or when they failed.
    fn keys_from(
        &mut self,
        connection: ConnectionId,
        schema: &str,
        table: &str,
        names: &[String],
    ) -> Option<Vec<ForeignKey>> {
        let entry = (connection, schema.to_owned());
        let keys = match self.explorer.foreign_keys.get(&entry) {
            Some(Loadable::Ready(keys)) => keys.clone(),
            Some(Loadable::Idle) | None => {
                self.explorer.foreign_keys.insert(entry, Loadable::Loading);
                self.backend
                    .load_foreign_keys(connection, schema.to_owned());
                return None;
            }
            Some(Loadable::Loading | Loadable::Failed(_)) => return None,
        };
        Some(
            keys.iter()
                .filter(|key| key.schema == schema && key.table == table)
                .filter(|key| key.columns.iter().all(|c| names.contains(c)))
                .cloned()
                .collect(),
        )
    }

    /// The tab's table and its foreign keys, loading what is missing.
    fn reference_target(&mut self, tab_id: TabId) -> Option<Target> {
        let tab = self.workspace.get(tab_id)?;
        if tab.running.as_ref().is_some_and(|r| !r.append) {
            return None;
        }
        let Some(Outcome::Done { output }) = &tab.outcome else {
            return None;
        };
        let set = output.sets.get(tab.active_set)?;
        if !set.has_rows() {
            return None;
        }
        let connection = tab.connection?;
        let source = tab.source.clone()?;
        let names: Vec<String> = set.columns.iter().map(|c| c.name.clone()).collect();
        let numeric: Vec<bool> = set.columns.iter().map(|c| c.numeric).collect();
        if !self.explorer.link(connection).is_online() {
            return None;
        }
        let Found::Table(schema, relation) = self.find_table(connection, &source) else {
            return None;
        };
        let keys = self.keys_from(connection, &schema, &relation, &names)?;
        if keys.is_empty() {
            return None;
        }
        Some(Target {
            connection,
            keys,
            names,
            numeric,
        })
    }

    /// Opens, under `row` of `tab_id`'s grid, the rows the key at `column`
    /// points at; following the same key of the same row again closes it.
    pub(super) fn follow_key(&mut self, tab_id: TabId, row: usize, column: usize) {
        let Some(target) = self.reference_target(tab_id) else {
            return;
        };
        let Some(values) = self.result_row(tab_id, row) else {
            return;
        };
        let source = Source {
            keys: &target.keys,
            names: &target.names,
            numeric: &target.numeric,
            row: &values,
        };
        let open = self.reference.as_ref().is_some_and(|r| {
            r.tab == tab_id
                && r.row == row
                && r.steps.first().is_some_and(|step| {
                    target
                        .names
                        .get(column)
                        .is_some_and(|name| step.key.columns.contains(name))
                })
        });
        if open {
            self.reference = None;
            return;
        }
        let Some(step) = self.step(target.connection, &source, column) else {
            return;
        };
        self.reference = Some(Reference {
            tab: tab_id,
            connection: target.connection,
            row,
            cells: values,
            steps: vec![step],
        });
    }

    /// Follows the key at `row`, `column` of the rows the extra row shows at
    /// `depth`, dropping the steps after it.
    pub(super) fn follow_nested_key(
        &mut self,
        tab_id: TabId,
        depth: usize,
        row: usize,
        column: usize,
    ) {
        let Some(reference) = self.reference.as_ref().filter(|r| r.tab == tab_id) else {
            return;
        };
        let connection = reference.connection;
        let Some(step) = reference.steps.get(depth) else {
            return;
        };
        let Lookup::Found(set) = &step.lookup else {
            return;
        };
        let Some(cells) = set.rows.row(row) else {
            return;
        };
        let values: Vec<Option<String>> = cells.iter().map(|v| v.map(str::to_owned)).collect();
        let names: Vec<String> = set.columns.iter().map(|c| c.name.clone()).collect();
        let numeric: Vec<bool> = set.columns.iter().map(|c| c.numeric).collect();
        let (schema, table) = (step.key.ref_schema.clone(), step.key.ref_table.clone());
        let Some(keys) = self.keys_from(connection, &schema, &table, &names) else {
            return;
        };
        let source = Source {
            keys: &keys,
            names: &names,
            numeric: &numeric,
            row: &values,
        };
        let Some(next) = self.step(connection, &source, column) else {
            return;
        };
        if let Some(reference) = &mut self.reference {
            reference.steps.truncate(depth + 1);
            reference.steps.push(next);
        }
    }

    /// Shows the step at `depth` again, dropping the ones after it.
    pub(super) fn back_to_reference(&mut self, tab_id: TabId, depth: usize) {
        if let Some(reference) = self.reference.as_mut().filter(|r| r.tab == tab_id) {
            reference.steps.truncate(depth + 1);
        }
    }

    /// Reads the rows the key `source` has at `column` points at; `None`
    /// when no key takes the column, or the row's key has a NULL.
    fn step(
        &mut self,
        connection: ConnectionId,
        source: &Source<'_>,
        column: usize,
    ) -> Option<Step> {
        let name = source.names.get(column)?;
        let key = source.keys.iter().find(|key| key.columns.contains(name))?;
        let mut values = Vec::with_capacity(key.columns.len());
        let mut numbers = Vec::with_capacity(key.columns.len());
        for name in &key.columns {
            let index = source.names.iter().position(|n| n == name)?;
            values.push(source.row.get(index).cloned().flatten()?);
            numbers.push(source.numeric[index]);
        }
        let lookup = match self.reference_sql(connection, key, &values, &numbers) {
            Some(sql) => {
                let options = ExecOptions {
                    cache: CachePolicy::Use,
                    page: Some(Page {
                        offset: 0,
                        limit: ROWS,
                    }),
                };
                Lookup::Loading(self.backend.execute(connection, sql, options))
            }
            None => Lookup::Failed(
                "The value holds characters that cannot be sent safely in a query.".to_owned(),
            ),
        };
        Some(Step {
            key: key.clone(),
            values,
            lookup,
            links: Vec::new(),
        })
    }

    /// `SELECT * FROM` the referenced table, for the rows with `values`.
    fn reference_sql(
        &self,
        connection: ConnectionId,
        key: &ForeignKey,
        values: &[String],
        numbers: &[bool],
    ) -> Option<String> {
        let dialect = self.dialect(connection);
        let mut filter = Vec::with_capacity(values.len());
        for ((column, value), numeric) in key.ref_columns.iter().zip(values).zip(numbers) {
            let literal = if *numeric && value.parse::<f64>().is_ok_and(f64::is_finite) {
                value.clone()
            } else {
                violet_core::string_literal(value)?
            };
            filter.push(format!("{} = {literal}", dialect.quote_ident(column)));
        }
        self.backend.browse_sql(
            connection,
            &key.ref_schema,
            &key.ref_table,
            Some(&filter.join(" AND ")),
            None,
        )
    }

    /// Whether `request` is a running lookup.
    pub(super) fn awaits_reference(&self, request: RequestId) -> bool {
        self.reference.as_ref().is_some_and(|r| {
            r.steps
                .iter()
                .any(|step| matches!(step.lookup, Lookup::Loading(running) if running == request))
        })
    }

    pub(super) fn reference_finished(&mut self, request: RequestId, result: Result<QueryOutput>) {
        let Some(step) = self.reference.as_mut().and_then(|r| {
            r.steps
                .iter_mut()
                .find(|step| matches!(step.lookup, Lookup::Loading(running) if running == request))
        }) else {
            return;
        };
        step.lookup = match result {
            Ok(mut output) if !output.sets.is_empty() => Lookup::Found(output.sets.swap_remove(0)),
            Ok(_) => Lookup::Failed("The query returned nothing.".to_owned()),
            Err(error) => Lookup::Failed(error.message),
        };
    }
}

struct Target {
    connection: ConnectionId,
    /// The foreign keys from the tab's table whose columns are all in the
    /// result.
    keys: Vec<ForeignKey>,
    names: Vec<String>,
    numeric: Vec<bool>,
}

/// The height of the extra row `reference` opens.
pub fn height(reference: &Reference) -> f32 {
    let body = match reference.steps.last().map(|step| &step.lookup) {
        Some(Lookup::Found(set)) if !set.rows.is_empty() => {
            let shown = set.rows.len().min(SHOWN);
            let more = if set.rows.len() > SHOWN {
                ROW_HEIGHT
            } else {
                0.0
            };
            // The header, the rows, and room for the scroll bar.
            ROW_HEIGHT * (shown + 1) as f32 + more + MARGIN
        }
        _ => ROW_HEIGHT + 4.0,
    };
    TITLE_HEIGHT + body + 2.0 * MARGIN
}

/// The extra row: `rect` is its part on screen, across the grid's width;
/// `clip` what of it the grid shows.
pub fn show(ui: &mut Ui, rect: Rect, clip: Rect, reference: &Reference, actions: &mut Vec<Action>) {
    let palette = Palette::current(ui.ctx());
    let Some(step) = reference.steps.last() else {
        return;
    };
    let depth = reference.steps.len() - 1;
    // A layer of its own just above the grid's: the grid paints its column
    // lines and places their resize handles over its rows after them.
    let layer = egui::LayerId::new(
        ui.layer_id().order,
        egui::Id::new(("reference-row", reference.tab)),
    );
    ui.ctx().set_sublayer(ui.layer_id(), layer);
    let mut ui = ui.new_child(
        UiBuilder::new()
            .id_salt(("reference-row", reference.tab))
            .layer_id(layer)
            .max_rect(rect)
            .layout(Layout::top_down(Align::Min)),
    );
    ui.set_clip_rect(clip);
    ui.painter()
        .rect_filled(rect, CornerRadius::ZERO, palette.panel);
    ui.painter().vline(
        rect.left() + 1.0,
        rect.y_range(),
        Stroke::new(2.0, palette.accent),
    );
    for y in [rect.top(), rect.bottom()] {
        ui.painter()
            .hline(rect.x_range(), y, Stroke::new(1.0, palette.outline));
    }
    let inner = rect.shrink2(Vec2::new(MARGIN + 6.0, MARGIN));
    let mut ui = ui.new_child(
        UiBuilder::new()
            .max_rect(inner)
            .layout(Layout::top_down(Align::Min)),
    );
    ui.spacing_mut().item_spacing.y = 0.0;

    ui.allocate_ui_with_layout(
        Vec2::new(inner.width(), TITLE_HEIGHT),
        Layout::left_to_right(Align::Center),
        |ui| {
            ui.set_min_width(inner.width());
            title(ui, reference, step, actions);
        },
    );

    let set = match &step.lookup {
        Lookup::Loading(_) => {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(14.0).color(palette.accent));
                ui.label(RichText::new("Reading the referenced row…").color(palette.dim));
            });
            return;
        }
        Lookup::Failed(error) => {
            ui.label(RichText::new(error).color(palette.danger));
            return;
        }
        Lookup::Found(set) => set,
    };
    if set.rows.is_empty() {
        ui.label(
            RichText::new(format!("No row of {} has this key.", step.key.ref_table))
                .color(palette.dim),
        );
        return;
    }
    egui::ScrollArea::horizontal()
        .id_salt(("reference-grid", reference.tab, depth))
        .auto_shrink([false, true])
        .show(&mut ui, |ui| {
            grid(ui, reference.tab, depth, step, set, actions);
        });
}

/// `customers › countries`, `id = 1`, the key's name, and the buttons.
fn title(ui: &mut Ui, reference: &Reference, step: &Step, actions: &mut Vec<Action>) {
    let palette = Palette::current(ui.ctx());
    ui.spacing_mut().item_spacing.x = 5.0;
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(13.0), Sense::hover());
    widgets::paint_icon(ui, Icon::Key, rect, palette.accent);
    let last = reference.steps.len() - 1;
    for (depth, followed) in reference.steps.iter().enumerate() {
        if depth > 0 {
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
            widgets::paint_icon(ui, Icon::ChevronRight, rect, palette.dim);
        }
        let name = &followed.key.ref_table;
        if depth == last {
            ui.label(
                RichText::new(name)
                    .font(theme::semibold(13.0))
                    .color(palette.text),
            );
        } else {
            let response = ui
                .push_id(("reference-crumb", depth), |ui| {
                    ui.add(
                        egui::Label::new(RichText::new(name).color(palette.accent))
                            .sense(Sense::click()),
                    )
                })
                .inner
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text(format!("Back to {name}"));
            if response.clicked() {
                actions.push(Action::BackToReference(reference.tab, depth));
            }
        }
    }
    let key = &step.key;
    let condition: Vec<String> = key
        .ref_columns
        .iter()
        .zip(&step.values)
        .map(|(column, value)| format!("{column} = {value}"))
        .collect();
    ui.add_space(6.0);
    ui.label(
        RichText::new(condition.join(" AND "))
            .font(theme::mono(12.0))
            .color(palette.secondary),
    );
    ui.label(
        RichText::new(format!("via {}", key.name))
            .font(theme::mono(10.5))
            .color(palette.dim),
    )
    .on_hover_text(format!(
        "{} ({}) → {}.{} ({})",
        key.name,
        key.columns.join(", "),
        key.ref_schema,
        key.ref_table,
        key.ref_columns.join(", ")
    ));
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        if widgets::icon_button(ui, Icon::Close, "Close").clicked() {
            actions.push(Action::CloseReference);
        }
        if widgets::icon_button(ui, Icon::Table, &format!("Open {}", key.ref_table)).clicked() {
            actions.push(Action::OpenData {
                connection: reference.connection,
                schema: key.ref_schema.clone(),
                relation: key.ref_table.clone(),
            });
        }
    });
}

/// The rows found, as a small grid: the key's columns marked, and the
/// referenced table's own foreign keys as links.
fn grid(
    ui: &mut Ui,
    tab: TabId,
    depth: usize,
    step: &Step,
    set: &ResultSet,
    actions: &mut Vec<Action>,
) {
    let palette = Palette::current(ui.ctx());
    let looks: Vec<Look> = set.columns.iter().map(Look::of).collect();
    let marked: Vec<bool> = set
        .columns
        .iter()
        .map(|column| step.key.ref_columns.contains(&column.name))
        .collect();
    let widths: Vec<f32> = set
        .columns
        .iter()
        .enumerate()
        .map(|(index, column)| super::results::column_width(set, index, &column.name))
        .collect();
    ui.spacing_mut().item_spacing = Vec2::ZERO;
    // A cell's own area to paint in, clipped to what the grid shows.
    let cell_ui = |ui: &mut Ui, rect: Rect| {
        let mut cell = ui.new_child(
            UiBuilder::new()
                .max_rect(rect)
                .layout(Layout::left_to_right(Align::Center)),
        );
        cell.set_clip_rect(rect.intersect(ui.clip_rect()));
        cell
    };

    ui.horizontal(|ui| {
        for (index, column) in set.columns.iter().enumerate() {
            let (rect, _) =
                ui.allocate_exact_size(Vec2::new(widths[index], ROW_HEIGHT), Sense::hover());
            let fill = if marked[index] {
                palette.accent_soft
            } else {
                palette.surface
            };
            ui.painter().rect_filled(rect, CornerRadius::ZERO, fill);
            let mut cell = cell_ui(ui, rect);
            cell.spacing_mut().item_spacing.x = 4.0;
            cell.add_space(6.0);
            if marked[index] || step.links.get(index).copied().unwrap_or(false) {
                let (icon, _) = cell.allocate_exact_size(Vec2::splat(11.0), Sense::hover());
                widgets::paint_icon(&cell, Icon::Key, icon, palette.accent);
            }
            cell.label(
                RichText::new(&column.name)
                    .font(theme::semibold(12.0))
                    .color(if marked[index] {
                        palette.accent
                    } else {
                        palette.text
                    }),
            );
            if let Some(kind) = &column.type_name {
                cell.label(
                    RichText::new(kind)
                        .font(theme::mono(10.5))
                        .color(palette.dim),
                );
            }
        }
    });

    for index in 0..set.rows.len().min(SHOWN) {
        ui.horizontal(|ui| {
            for (column, meta) in set.columns.iter().enumerate() {
                let value = set.rows.get(index, column).flatten();
                let (rect, _) =
                    ui.allocate_exact_size(Vec2::new(widths[column], ROW_HEIGHT), Sense::hover());
                let response = ui.interact(
                    rect,
                    egui::Id::new(("reference-cell", tab, depth, index, column)),
                    Sense::click(),
                );
                if marked[column] {
                    ui.painter()
                        .rect_filled(rect, CornerRadius::ZERO, palette.accent_soft);
                }
                let cell = cell_ui(ui, rect);
                let (shown, color) = match value {
                    Some(text) => (Cell::Value(text), palette.text),
                    None => (Cell::Null, palette.dim),
                };
                let link = step.links.get(column).copied().unwrap_or(false);
                let chip =
                    cells::paint(&cell, shown, looks[column], meta.numeric, color, link).link;
                let on_chip = |pointer: Option<egui::Pos2>| {
                    pointer
                        .zip(chip)
                        .is_some_and(|(pointer, chip)| chip.contains(pointer))
                };
                if on_chip(response.hover_pos()) {
                    response.ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
                }
                if response.clicked() && on_chip(response.interact_pointer_pos()) {
                    actions.push(Action::FollowNestedKey(tab, depth, index, column));
                }
                if response.hovered()
                    && let Some(text) = value
                {
                    let text = match looks[column] {
                        Look::Geometry => cells::geometry_text(text),
                        Look::Text | Look::Boolean => std::borrow::Cow::Borrowed(text),
                    };
                    if text.len() > 40 || text.contains('\n') {
                        response.clone().on_hover_ui(|ui| {
                            ui.set_max_width(480.0);
                            ui.label(RichText::new(text).font(theme::mono(12.0)));
                        });
                    }
                }
                response.context_menu(|ui| {
                    if chip.is_some()
                        && super::sidebar::menu_item(ui, Icon::ArrowUpRight, "Open referenced row")
                    {
                        actions.push(Action::FollowNestedKey(tab, depth, index, column));
                    }
                    if super::sidebar::menu_item(ui, Icon::Copy, "Copy value") {
                        actions.push(Action::Copy(value.unwrap_or_default().to_owned()));
                    }
                    if looks[column] == Look::Geometry
                        && let Some(wkt) = value.and_then(crate::geometry::wkt)
                        && super::sidebar::menu_item(ui, Icon::Copy, "Copy as WKT")
                    {
                        actions.push(Action::Copy(wkt));
                    }
                    if super::sidebar::menu_item(ui, Icon::Columns, "Copy column name") {
                        actions.push(Action::Copy(meta.name.clone()));
                    }
                });
            }
        });
    }
    let more = set.rows.len().saturating_sub(SHOWN);
    if more > 0 {
        ui.add_space(4.0);
        ui.label(
            RichText::new(format!(
                "{more} more {} — open the table to see them all",
                if more == 1 { "row" } else { "rows" }
            ))
            .color(palette.dim),
        );
    }
}
