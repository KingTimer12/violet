//! The row a foreign key points at, beside the grid.
//!
//! Picking a cell of a key column reads the row it references and shows it
//! in a side panel, with the referenced cell marked. It works where the
//! rows map back to one table (as for editing), whose foreign keys the
//! catalog lists.

use egui::{Align, CornerRadius, Layout, RichText, Sense, Stroke, Ui, Vec2};
use violet_app::{CachePolicy, ExecOptions, RequestId};
use violet_core::{ConnectionId, Dialect, ForeignKey, Page, QueryOutput, Result, ResultSet};

use super::VioletApp;
use super::action::Action;
use super::edit_flow::Found;
use super::icons::Icon;
use super::state::Loadable;
use super::theme::{self, Palette};
use super::widgets::{self, Tone};
use super::workspace::{Outcome, TabId};

/// The referenced row of the selected cell.
pub struct Reference {
    pub tab: TabId,
    pub connection: ConnectionId,
    /// The selected cell: row and column.
    pub cell: (usize, usize),
    pub key: ForeignKey,
    /// The values of the key's columns, in key order.
    pub values: Vec<String>,
    /// The key column the selected cell is in, by its place in the key.
    pub part: usize,
    pub lookup: Lookup,
    /// Whether the marked cell has been scrolled into view.
    scrolled: bool,
}

pub enum Lookup {
    Loading(RequestId),
    Found(ResultSet),
    Failed(String),
}

impl Reference {
    fn same(&self, tab: TabId, cell: (usize, usize), key: &ForeignKey, values: &[String]) -> bool {
        self.tab == tab && self.cell == cell && self.key == *key && self.values == values
    }
}

/// What the grid needs to know about the tab's foreign keys.
#[derive(Default)]
pub struct Links {
    /// Per result column: whether it is part of a foreign key.
    pub columns: Vec<bool>,
}

impl VioletApp {
    /// Follows the selected cell of `tab_id`: when it is in a foreign key
    /// column, reads the row it points at. Returns which columns are keys.
    pub(super) fn sync_reference(&mut self, tab_id: TabId) -> Links {
        let Some(found) = self.reference_target(tab_id) else {
            self.reference = None;
            return Links::default();
        };
        let Target {
            connection,
            keys,
            names,
            numeric,
            selected,
        } = found;
        let links = Links {
            columns: names
                .iter()
                .map(|name| keys.iter().any(|key| key.columns.contains(name)))
                .collect(),
        };
        let Some((cell, row)) = selected else {
            self.reference = None;
            return links;
        };
        let column = &names[cell.1];
        let Some(key) = keys.iter().find(|key| key.columns.contains(column)) else {
            self.reference = None;
            return links;
        };
        // Every key column's value, from the selected row; NULL points at
        // nothing.
        let mut values = Vec::with_capacity(key.columns.len());
        let mut numbers = Vec::with_capacity(key.columns.len());
        for name in &key.columns {
            let Some(index) = names.iter().position(|n| n == name) else {
                self.reference = None;
                return links;
            };
            let Some(value) = row.get(index).cloned().flatten() else {
                self.reference = None;
                return links;
            };
            values.push(value);
            numbers.push(numeric[index]);
        }
        if self
            .reference
            .as_ref()
            .is_some_and(|r| r.same(tab_id, cell, key, &values))
        {
            return links;
        }
        let part = key
            .columns
            .iter()
            .position(|name| name == column)
            .unwrap_or(0);
        let lookup = match self.reference_sql(connection, key, &values, &numbers) {
            Some(sql) => {
                let options = ExecOptions {
                    cache: CachePolicy::Use,
                    page: Some(Page {
                        offset: 0,
                        limit: 1,
                    }),
                };
                Lookup::Loading(self.backend.execute(connection, sql, options))
            }
            None => Lookup::Failed(
                "The value holds characters that cannot be sent safely in a query.".to_owned(),
            ),
        };
        self.reference = Some(Reference {
            tab: tab_id,
            connection,
            cell,
            key: key.clone(),
            values,
            part,
            lookup,
            scrolled: false,
        });
        links
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
        let selected = tab.selected_cell.and_then(|(row, column)| {
            let cells = set.rows.row(row)?;
            let values = cells.iter().map(|v| v.map(str::to_owned)).collect();
            (column < names.len()).then_some(((row, column), values))
        });
        if !self.explorer.link(connection).is_online() {
            return None;
        }
        let Found::Table(schema, relation) = self.find_table(connection, &source) else {
            return None;
        };
        let keys = match self
            .explorer
            .foreign_keys
            .get(&(connection, schema.clone()))
        {
            Some(Loadable::Ready(keys)) => keys.clone(),
            Some(Loadable::Idle) | None => {
                self.explorer
                    .foreign_keys
                    .insert((connection, schema.clone()), Loadable::Loading);
                self.backend.load_foreign_keys(connection, schema);
                return None;
            }
            Some(Loadable::Loading | Loadable::Failed(_)) => return None,
        };
        let keys: Vec<ForeignKey> = keys
            .iter()
            .filter(|key| key.schema == schema && key.table == relation)
            .filter(|key| key.columns.iter().all(|c| names.contains(c)))
            .cloned()
            .collect();
        if keys.is_empty() {
            return None;
        }
        Some(Target {
            connection,
            keys,
            names,
            numeric,
            selected,
        })
    }

    /// `SELECT * FROM` the referenced table, for the row with `values`.
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

    /// Whether `request` is the running lookup.
    pub(super) fn awaits_reference(&self, request: RequestId) -> bool {
        self.reference
            .as_ref()
            .is_some_and(|r| matches!(r.lookup, Lookup::Loading(running) if running == request))
    }

    pub(super) fn reference_finished(&mut self, result: Result<QueryOutput>) {
        let Some(reference) = &mut self.reference else {
            return;
        };
        reference.lookup = match result {
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
    /// The selected cell, and its row's values.
    selected: Option<Selected>,
}

/// A cell (row and column) and its row's values.
type Selected = ((usize, usize), Vec<Option<String>>);

/// The side panel.
pub fn show(ui: &mut Ui, reference: &mut Reference, actions: &mut Vec<Action>) {
    let palette = Palette::current(ui.ctx());
    let key = &reference.key;
    egui::Frame::new()
        .inner_margin(egui::Margin::symmetric(12, 10))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(14.0), Sense::hover());
                widgets::paint_icon(ui, Icon::Key, rect, palette.accent);
                widgets::caption(ui, "Referenced row");
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if widgets::icon_button(ui, Icon::Close, "Close").clicked() {
                        actions.push(Action::CloseReference(reference.tab));
                    }
                });
            });
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(&key.ref_table)
                        .font(theme::semibold(14.0))
                        .color(palette.text),
                );
                ui.label(RichText::new(&key.ref_schema).color(palette.dim));
            });
            let condition: Vec<String> = key
                .ref_columns
                .iter()
                .zip(&reference.values)
                .map(|(column, value)| format!("{column} = {value}"))
                .collect();
            ui.add(
                egui::Label::new(
                    RichText::new(condition.join(" AND "))
                        .font(theme::mono(12.0))
                        .color(palette.secondary),
                )
                .wrap(),
            );
            ui.add_space(4.0);
            ui.label(
                RichText::new(format!("via {} ({})", key.name, key.columns.join(", ")))
                    .font(theme::mono(10.5))
                    .color(palette.dim),
            );
            ui.add_space(8.0);
            if widgets::button(ui, Tone::Secondary, Some(Icon::Table), "Open table").clicked() {
                actions.push(Action::OpenData {
                    connection: reference.connection,
                    schema: key.ref_schema.clone(),
                    relation: key.ref_table.clone(),
                });
            }
        });
    ui.painter().hline(
        ui.max_rect().x_range(),
        ui.cursor().top(),
        Stroke::new(1.0, palette.outline),
    );

    let set = match &reference.lookup {
        Lookup::Loading(_) => {
            ui.vertical_centered(|ui| {
                ui.add_space(24.0);
                ui.add(egui::Spinner::new().size(20.0).color(palette.accent));
            });
            return;
        }
        Lookup::Failed(error) => {
            ui.add_space(12.0);
            ui.horizontal_wrapped(|ui| {
                ui.add_space(12.0);
                ui.label(RichText::new(error).color(palette.danger));
            });
            return;
        }
        Lookup::Found(set) => set,
    };
    let Some(row) = set.rows.row(0) else {
        widgets::empty_state(
            ui,
            Icon::Rows,
            "No row found",
            "No row of the referenced table has this key.",
        );
        return;
    };
    let marked = key.ref_columns.get(reference.part);
    let mut scroll = !reference.scrolled;
    egui::ScrollArea::vertical()
        .id_salt("reference-row")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add_space(6.0);
            for (meta, value) in set.columns.iter().zip(row.iter()) {
                let is_marked = marked.is_some_and(|name| *name == meta.name);
                let in_key = key.ref_columns.contains(&meta.name);
                let frame = egui::Frame::new()
                    .inner_margin(egui::Margin::symmetric(10, 6))
                    .outer_margin(egui::Margin::symmetric(8, 2))
                    .corner_radius(CornerRadius::same(6));
                let frame = if is_marked {
                    frame
                        .fill(palette.accent_soft)
                        .stroke(Stroke::new(1.0, palette.accent))
                } else {
                    frame
                };
                let response = frame
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            if in_key {
                                let (rect, _) =
                                    ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
                                widgets::paint_icon(ui, Icon::Key, rect, palette.accent);
                            }
                            ui.label(RichText::new(&meta.name).font(theme::semibold(11.5)).color(
                                if is_marked {
                                    palette.accent
                                } else {
                                    palette.secondary
                                },
                            ));
                            if let Some(kind) = &meta.type_name {
                                ui.label(
                                    RichText::new(kind)
                                        .font(theme::mono(10.5))
                                        .color(palette.dim),
                                );
                            }
                        });
                        let text = match value {
                            Some(text) => RichText::new(text)
                                .font(theme::mono(12.5))
                                .color(palette.text),
                            None => RichText::new("NULL")
                                .font(theme::mono(11.5))
                                .italics()
                                .color(palette.dim),
                        };
                        ui.add(egui::Label::new(text).wrap().selectable(true));
                    })
                    .response
                    .interact(Sense::click());
                if is_marked && scroll {
                    response.scroll_to_me(Some(Align::Center));
                    scroll = false;
                }
                response.context_menu(|ui| {
                    if super::sidebar::menu_item(ui, Icon::Copy, "Copy value") {
                        actions.push(Action::Copy(value.unwrap_or_default().to_owned()));
                    }
                    if super::sidebar::menu_item(ui, Icon::Columns, "Copy column name") {
                        actions.push(Action::Copy(meta.name.clone()));
                    }
                });
            }
            ui.add_space(6.0);
        });
    reference.scrolled = true;
}
