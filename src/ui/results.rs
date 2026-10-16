//! The results pane: a grid per result set, command summaries, errors, and
//! the progress of a running query.

use std::borrow::Cow;
use std::collections::BTreeMap;

use egui::text::{LayoutJob, TextWrapping};
use egui::{
    Align, Color32, CornerRadius, Layout, Pos2, RichText, Sense, Stroke, TextFormat, Ui, Vec2,
};
use egui_extras::{Column, TableBuilder};
use violet_core::{DbError, ResultSet};

use super::action::Action;
use super::edit_flow::{EditTarget, Editing};
use super::icons::Icon;
use super::selection::{Mode, Selection};
use super::theme::{self, Palette};
use super::widgets::{self, Tone};
use super::workspace::{
    Count, Executed, Outcome, Paging, QueryTab, TabKind, format_duration, thousands,
};
use crate::export::{to_delimited, write_row};
use crate::sql;

const ROW_HEIGHT: f32 = 26.0;
/// Characters shown per cell; the full value is a click away.
const CELL_PREVIEW: usize = 300;
/// Room between a cell's edge and its text.
const CELL_PADDING: f32 = 6.0;

/// What a cell shows.
#[derive(Clone, Copy)]
enum Cell<'a> {
    Value(&'a str),
    Null,
    RowNumber(usize),
}

/// Paints `cell` in the cell `ui` stands for, cut to its width and centred
/// vertically. Painted directly rather than as a label widget; the layout
/// comes from egui's own cache, which knows when the font atlas changes.
fn paint_cell(ui: &Ui, cell: Cell<'_>, right_aligned: bool, color: Color32) {
    let rect = ui.max_rect();
    let mut format = TextFormat::simple(theme::mono(12.5), color);
    let text = match cell {
        Cell::Value(text) => preview(text).into_owned(),
        Cell::Null => {
            format.font_id = theme::mono(11.5);
            format.italics = true;
            "NULL".to_owned()
        }
        Cell::RowNumber(row) => {
            format.font_id = theme::mono(11.0);
            (row + 1).to_string()
        }
    };
    let mut job = LayoutJob::single_section(text, format);
    job.wrap = TextWrapping::truncate_at_width((rect.width() - 2.0 * CELL_PADDING).max(0.0));
    let galley = ui.painter().layout_job(job);
    let x = if right_aligned {
        rect.right() - CELL_PADDING - galley.size().x
    } else {
        rect.left() + CELL_PADDING
    };
    let y = rect.center().y - galley.size().y / 2.0;
    ui.painter().galley(Pos2::new(x, y), galley, color);
}

#[hotpath::measure]
pub fn show(
    ui: &mut Ui,
    tab: &mut QueryTab,
    editing: &Editing,
    links: &[bool],
    actions: &mut Vec<Action>,
) {
    let palette = Palette::current(ui.ctx());
    // Fetching another page keeps the rows on screen; the footer shows it.
    if let Some(running) = &tab.running
        && !running.append
    {
        let elapsed = running.started.elapsed();
        ui.vertical_centered(|ui| {
            ui.add_space((ui.available_height() / 2.0 - 50.0).max(10.0));
            ui.add(egui::Spinner::new().size(26.0).color(palette.accent));
            ui.add_space(8.0);
            ui.label(
                RichText::new(format!("Running… {}", format_duration(elapsed)))
                    .color(palette.secondary),
            );
            ui.add_space(8.0);
            let label = if running.cancelling {
                "Cancelling…"
            } else {
                "Cancel"
            };
            let response = ui.add_enabled_ui(!running.cancelling, |ui| {
                widgets::button(ui, Tone::Secondary, Some(Icon::Stop), label)
            });
            if response.inner.clicked() {
                actions.push(Action::Cancel(tab.id));
            }
        });
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(100));
        return;
    }

    match &tab.outcome {
        None => {
            let hint = ui
                .ctx()
                .format_shortcut(&crate::ui::shortcuts::RUN_STATEMENT);
            widgets::empty_state(
                ui,
                Icon::Rows,
                "No results yet",
                &format!("Press {hint} to run the statement under the cursor."),
            );
        }
        Some(Outcome::Failed { error, executed }) => {
            error_card(ui, error, executed, tab.sql.as_str())
        }
        Some(Outcome::Done { output, .. }) => {
            if output.sets.is_empty() {
                widgets::empty_state(
                    ui,
                    Icon::CircleCheck,
                    "Done",
                    "The statement returned nothing.",
                );
                return;
            }
            let count = output.sets.len();
            tab.active_set = tab.active_set.min(count - 1);
            let elapsed = output.elapsed;
            let set = &output.sets[tab.active_set];

            // Header: result set chips, then stats and actions.
            egui::Frame::new()
                .inner_margin(egui::Margin::symmetric(10, 6))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        if count > 1 {
                            let mut chosen = tab.active_set;
                            let labels: Vec<String> =
                                (1..=count).map(|n| format!("Result {n}")).collect();
                            let options: Vec<(usize, &str)> = labels
                                .iter()
                                .enumerate()
                                .map(|(i, l)| (i, l.as_str()))
                                .collect();
                            if widgets::segmented(ui, &mut chosen, &options) {
                                tab.active_set = chosen;
                                tab.grid_selection.clear();
                            }
                        } else {
                            let label = if set.has_rows() { "Result" } else { "Command" };
                            ui.label(
                                RichText::new(label)
                                    .font(theme::semibold(13.0))
                                    .color(palette.text),
                            );
                        }
                        if set.has_rows() {
                            edit_bar(ui, tab.id, tab.edits.len(), editing, actions);
                        }
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if set.has_rows() {
                                let export =
                                    widgets::icon_button(ui, Icon::Download, "Export rows…");
                                egui::Popup::menu(&export).show(|ui| {
                                    for format in crate::export::Format::ALL {
                                        if super::sidebar::menu_item(
                                            ui,
                                            Icon::Download,
                                            format.label(),
                                        ) {
                                            actions.push(Action::Export(tab.id, format));
                                        }
                                    }
                                });
                                if widgets::icon_button(ui, Icon::Copy, "Copy all rows as CSV")
                                    .clicked()
                                {
                                    actions.push(Action::Copy(to_delimited(set, ',')));
                                }
                            }
                            if tab.last_run.is_some()
                                && widgets::icon_button(
                                    ui,
                                    Icon::Refresh,
                                    "Read again from the server",
                                )
                                .clicked()
                            {
                                actions.push(Action::RefreshTab(tab.id));
                            }
                            ui.label(RichText::new(format_duration(elapsed)).color(palette.dim));
                            if let Some(age) = output.cached {
                                widgets::pill(
                                    ui,
                                    &format!("Cached · {}", format_age(age)),
                                    palette.success,
                                    palette.success.gamma_multiply(0.15),
                                )
                                .on_hover_text(
                                    "Nothing changed on the server since these rows were read, \
                                     so they were shown from memory. Refresh to read them again.",
                                );
                            }
                            if set.has_rows() {
                                let more = tab.paging.as_ref().is_some_and(|p| p.has_more);
                                widgets::pill(
                                    ui,
                                    &format!(
                                        "{}{} rows",
                                        thousands(set.rows.len()),
                                        if more { "+" } else { "" }
                                    ),
                                    palette.accent,
                                    palette.accent_soft,
                                );
                            }
                            if set.truncated {
                                widgets::pill(
                                    ui,
                                    &format!("Limited to {}", thousands(set.rows.len())),
                                    palette.warning,
                                    palette.warning.gamma_multiply(0.15),
                                )
                                .on_hover_text(
                                    "The query returned more rows; only the first ones are kept.",
                                );
                            }
                        });
                    });
                });
            ui.painter().hline(
                ui.max_rect().x_range(),
                ui.cursor().top(),
                Stroke::new(1.0, palette.outline),
            );

            if set.has_rows() {
                if let Some(paging) = &tab.paging {
                    let fetching = tab.running.as_ref().is_some_and(|r| r.append);
                    egui::Panel::bottom(egui::Id::new(("paging", tab.id)))
                        .exact_size(34.0)
                        .frame(
                            egui::Frame::NONE
                                .fill(palette.panel)
                                .stroke(Stroke::new(1.0, palette.outline)),
                        )
                        .show_separator_line(false)
                        .show(ui, |ui| paging_bar(ui, tab.id, paging, fetching, actions));
                }
                let sort = match &tab.kind {
                    TabKind::Data(view) => Sorting::On(view.order.as_ref().and_then(|order| {
                        set.columns
                            .iter()
                            .position(|column| column.name == order.column)
                            .map(|index| (index, order.descending))
                    })),
                    // A single query is read a page at a time; sorting it
                    // reads it again, sorted by the server.
                    TabKind::Script if tab.paging.is_some() => Sorting::On(tab.sort),
                    TabKind::Script => Sorting::Off,
                };
                let target = match editing {
                    Editing::On(target) => Some(target),
                    Editing::Off(_) | Editing::Loading => None,
                };
                grid(
                    ui,
                    tab.id,
                    set,
                    sort,
                    &mut tab.grid_selection,
                    target,
                    &tab.edits,
                    links,
                    actions,
                );
            } else {
                let affected = set.affected.unwrap_or(0);
                widgets::empty_state(
                    ui,
                    Icon::CircleCheck,
                    "Statement executed",
                    &format!(
                        "{} rows affected",
                        thousands(usize::try_from(affected).unwrap_or(usize::MAX))
                    ),
                );
            }
        }
    }
}

/// DBeaver's result footer: how much is loaded, and buttons to read more
/// or count the rest.
fn paging_bar(ui: &mut Ui, tab: u64, paging: &Paging, fetching: bool, actions: &mut Vec<Action>) {
    let palette = Palette::current(ui.ctx());
    ui.horizontal_centered(|ui| {
        ui.add_space(10.0);
        let loaded = usize::try_from(paging.loaded).unwrap_or(usize::MAX);
        let fetched = if paging.has_more {
            format!("{} rows fetched", thousands(loaded))
        } else {
            format!("All {} rows fetched", thousands(loaded))
        };
        ui.label(RichText::new(fetched).color(palette.secondary));
        match &paging.total {
            Some(Count::Done(total)) => {
                let total = usize::try_from(*total).unwrap_or(usize::MAX);
                ui.label(RichText::new(format!("of {}", thousands(total))).color(palette.dim));
            }
            Some(Count::Running(_)) => {
                ui.add(egui::Spinner::new().size(12.0).color(palette.accent));
            }
            Some(Count::Failed(error)) => {
                ui.label(RichText::new("count failed").color(palette.danger))
                    .on_hover_text(error);
            }
            None => {}
        }
        if fetching {
            ui.add(egui::Spinner::new().size(12.0).color(palette.accent));
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.add_space(8.0);
            let idle = !fetching;
            let count_idle = !matches!(paging.total, Some(Count::Running(_)));
            if ui
                .add_enabled_ui(count_idle, |ui| {
                    widgets::icon_button(ui, Icon::Sequence, "Count all rows")
                })
                .inner
                .clicked()
            {
                actions.push(Action::CountRows(tab));
            }
            if ui
                .add_enabled_ui(idle && paging.has_more, |ui| {
                    widgets::icon_button(
                        ui,
                        Icon::ChevronsDown,
                        "Fetch all rows (up to the row limit)",
                    )
                })
                .inner
                .clicked()
            {
                actions.push(Action::FetchAll(tab));
            }
            let next = format!("Next {}", violet_core::Page::SIZE);
            if ui
                .add_enabled_ui(idle && paging.has_more, |ui| {
                    widgets::button(ui, Tone::Secondary, Some(Icon::ChevronDown), &next)
                })
                .inner
                .on_hover_text("Fetch the next page of rows")
                .clicked()
            {
                actions.push(Action::FetchMore(tab));
            }
        });
    });
}

/// `just now`, `42 s ago`, `3 min ago`.
fn format_age(age: std::time::Duration) -> String {
    match age.as_secs() {
        0..=1 => "just now".to_owned(),
        seconds @ 2..=59 => format!("{seconds} s ago"),
        seconds => format!("{} min ago", seconds / 60),
    }
}

#[hotpath::measure]
#[allow(clippy::too_many_arguments)]
fn grid(
    ui: &mut Ui,
    tab: u64,
    set: &ResultSet,
    sort: Sorting,
    selected: &mut Selection,
    target: Option<&EditTarget>,
    edits: &BTreeMap<(usize, usize), Option<String>>,
    links: &[bool],
    actions: &mut Vec<Action>,
) {
    let palette = Palette::current(ui.ctx());
    let number_width = 14.0 + 7.5 * set.rows.len().max(1).to_string().len() as f32;

    // Clicks pick cells as DBeaver does: Ctrl (⌘) adds, Shift extends.
    let mode = Mode::from_modifiers(ui.input(|input| input.modifiers));
    let columns = set.columns.len();

    // The platform shortcuts copy the picked cells and pick them all,
    // once the grid is in use and no text field takes the keys.
    if !selected.is_empty() && !ui.ctx().egui_wants_keyboard_input() {
        let copy = ui.input(|input| {
            input
                .events
                .iter()
                .any(|event| matches!(event, egui::Event::Copy))
        });
        if copy {
            actions.push(Action::Copy(copy_selection(set, edits, selected, false)));
        }
        if ui.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::A)) {
            selected.select_all(set.rows.len(), columns);
        }
    }

    egui::ScrollArea::horizontal()
        .id_salt(("grid-h", tab))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let mut table = TableBuilder::new(ui)
                .id_salt(("grid", tab))
                .striped(true)
                .resizable(true)
                .cell_layout(Layout::left_to_right(Align::Center))
                .auto_shrink([false, false])
                .min_scrolled_height(0.0)
                .max_scroll_height(f32::INFINITY)
                .column(Column::exact(number_width));
            for (index, column) in set.columns.iter().enumerate() {
                table = table.column(
                    Column::initial(column_width(set, index, &column.name))
                        .at_least(48.0)
                        .clip(true),
                );
            }
            table
                .header(30.0, |mut header| {
                    header.col(|ui| {
                        ui.painter().rect_filled(ui.max_rect(), 0.0, palette.panel);
                    });
                    for (index, column) in set.columns.iter().enumerate() {
                        let mut head = None;
                        header.col(|ui| {
                            ui.painter().rect_filled(ui.max_rect(), 0.0, palette.panel);
                            ui.add_space(6.0);
                            // A foreign key: picking a cell shows the row
                            // it points at.
                            if links.get(index).copied().unwrap_or(false) {
                                let (rect, _) =
                                    ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
                                widgets::paint_icon(ui, Icon::Key, rect, palette.accent);
                            }
                            ui.label(
                                RichText::new(&column.name)
                                    .font(theme::semibold(12.5))
                                    .color(palette.text),
                            );
                            // The sort arrow sits by the name, where narrow
                            // columns still show it.
                            let direction = match sort {
                                Sorting::On(Some((sorted, descending))) if sorted == index => {
                                    Some(descending)
                                }
                                Sorting::On(_) | Sorting::Off => None,
                            };
                            if let Some(descending) = direction {
                                let icon = if descending {
                                    Icon::ArrowDown
                                } else {
                                    Icon::ArrowUp
                                };
                                let (rect, _) =
                                    ui.allocate_exact_size(Vec2::splat(13.0), Sense::hover());
                                widgets::paint_icon(ui, icon, rect, palette.accent);
                            }
                            if let Some(kind) = &column.type_name {
                                ui.label(
                                    RichText::new(kind)
                                        .font(theme::mono(10.5))
                                        .color(palette.dim),
                                );
                            }
                            // The table's own response for a cell has no
                            // area: the header takes clicks itself.
                            head = Some(ui.interact(
                                ui.max_rect(),
                                ui.id().with("head"),
                                Sense::click(),
                            ));
                        });
                        let Some(head) = head else { continue };
                        let head = head.on_hover_cursor(egui::CursorIcon::PointingHand);
                        egui::Popup::menu(&head).show(|ui| {
                            let Sorting::On(current) = sort else {
                                ui.label(
                                    RichText::new(
                                        "Only the result of a single SELECT can be sorted.",
                                    )
                                    .color(palette.dim),
                                );
                                return;
                            };
                            let now = current
                                .filter(|(sorted, _)| *sorted == index)
                                .map(|(_, descending)| descending);
                            for (icon, label, descending) in [
                                (Icon::ArrowUp, "Sort ascending", false),
                                (Icon::ArrowDown, "Sort descending", true),
                            ] {
                                let label = if now == Some(descending) {
                                    format!("{label}  ✓")
                                } else {
                                    label.to_owned()
                                };
                                if super::sidebar::menu_item(ui, icon, &label) {
                                    actions.push(Action::SortBy(tab, index, Some(descending)));
                                }
                            }
                            if now.is_some()
                                && super::sidebar::menu_item(ui, Icon::Close, "Clear sort")
                            {
                                actions.push(Action::SortBy(tab, index, None));
                            }
                        });
                    }
                })
                .body(|body| {
                    body.rows(ROW_HEIGHT, set.rows.len(), |mut row| {
                        let index = row.index();
                        row.set_selected(selected.has_row(index));
                        // The row number picks the whole row.
                        let mut number = None;
                        row.col(|ui| {
                            paint_cell(ui, Cell::RowNumber(index), true, palette.dim);
                            number = Some(ui.interact(
                                ui.max_rect(),
                                ui.id().with("row-number"),
                                Sense::click(),
                            ));
                        });
                        if let Some(response) = number {
                            if response.clicked() {
                                selected.click_row(index, columns, mode);
                            }
                            if response.secondary_clicked() && !selected.has_row(index) {
                                selected.click_row(index, columns, Mode::Replace);
                            }
                            response.context_menu(|ui| {
                                rows_menu(ui, set, edits, selected, actions);
                            });
                        }
                        let Some(cells) = set.rows.row(index) else {
                            return;
                        };
                        for (column, value) in cells.iter().enumerate() {
                            let meta = &set.columns[column];
                            let edited = edits.get(&(index, column));
                            // A change not saved yet shows instead of the
                            // server's value.
                            let value = match edited {
                                Some(change) => change.as_deref(),
                                None => value,
                            };
                            let editable = target
                                .is_some_and(|t| t.editable.get(column).copied().unwrap_or(false));
                            let mut cell = None;
                            row.col(|ui| {
                                if edited.is_some() {
                                    ui.painter().rect_filled(
                                        ui.max_rect(),
                                        0.0,
                                        palette.warning.gamma_multiply(0.22),
                                    );
                                }
                                if selected.contains((index, column)) {
                                    // The cell clicked last gets the outline.
                                    let stroke = if selected.focus == Some((index, column)) {
                                        Stroke::new(1.0, palette.accent)
                                    } else {
                                        Stroke::NONE
                                    };
                                    ui.painter().rect(
                                        ui.max_rect().shrink(1.0),
                                        CornerRadius::same(3),
                                        palette.accent_soft,
                                        stroke,
                                        egui::StrokeKind::Inside,
                                    );
                                }
                                let (cell_text, color) = match value {
                                    None => (Cell::Null, palette.dim),
                                    Some(text) => (Cell::Value(text), palette.text),
                                };
                                paint_cell(ui, cell_text, meta.numeric, color);
                                // The cell is painted, not laid out, and the
                                // table's own response for it has no area:
                                // it takes clicks itself.
                                cell = Some(ui.interact(
                                    ui.max_rect(),
                                    ui.id().with("cell"),
                                    Sense::click(),
                                ));
                            });
                            let Some(response) = cell else {
                                continue;
                            };
                            if response.clicked() {
                                selected.click((index, column), mode);
                            }
                            // Right-clicking outside the selection picks the
                            // cell; inside, the menu acts on all of it.
                            if response.secondary_clicked() && !selected.contains((index, column)) {
                                selected.click((index, column), Mode::Replace);
                            }
                            if response.double_clicked() && editable {
                                actions.push(Action::EditCell(tab, index, column));
                            }
                            // Long values get a tooltip; only measured when hovered.
                            if response.hovered()
                                && let Some(text) = value
                                && (text.len() > 40 || text.contains('\n'))
                            {
                                response.clone().on_hover_ui(|ui| {
                                    ui.set_max_width(480.0);
                                    ui.label(RichText::new(text).font(theme::mono(12.0)));
                                });
                            }
                            response.context_menu(|ui| {
                                if editable {
                                    if super::sidebar::menu_item(ui, Icon::Pencil, "Edit value…")
                                    {
                                        actions.push(Action::EditCell(tab, index, column));
                                    }
                                    if super::sidebar::menu_item(ui, Icon::Close, "Set to NULL") {
                                        actions.push(Action::SetCell(tab, index, column, None));
                                    }
                                    if edited.is_some()
                                        && super::sidebar::menu_item(
                                            ui,
                                            Icon::Refresh,
                                            "Revert change",
                                        )
                                    {
                                        actions.push(Action::RevertCell(tab, index, column));
                                    }
                                    ui.separator();
                                }
                                let label = match selected.len() {
                                    0 | 1 => "Copy value".to_owned(),
                                    count => format!("Copy {} cells", thousands(count)),
                                };
                                if super::sidebar::menu_item(ui, Icon::Copy, &label) {
                                    actions.push(Action::Copy(copy_selection(
                                        set, edits, selected, false,
                                    )));
                                }
                                if super::sidebar::menu_item(
                                    ui,
                                    Icon::Copy,
                                    "Copy with column names",
                                ) {
                                    actions.push(Action::Copy(copy_selection(
                                        set, edits, selected, true,
                                    )));
                                }
                                ui.separator();
                                rows_menu(ui, set, edits, selected, actions);
                                ui.separator();
                                if super::sidebar::menu_item(ui, Icon::Columns, "Copy column name")
                                {
                                    actions.push(Action::Copy(meta.name.clone()));
                                }
                            });
                        }
                    });
                });
        });
}

/// Copying the whole rows that hold picked cells.
fn rows_menu(
    ui: &mut Ui,
    set: &ResultSet,
    edits: &BTreeMap<(usize, usize), Option<String>>,
    selected: &Selection,
    actions: &mut Vec<Action>,
) {
    let rows = selected.rows();
    let noun = if rows.len() == 1 { "row" } else { "rows" };
    let count = if rows.len() > 1 {
        format!("{} ", thousands(rows.len()))
    } else {
        String::new()
    };
    if super::sidebar::menu_item(ui, Icon::Rows, &format!("Copy {count}{noun}")) {
        actions.push(Action::Copy(copy_rows(set, edits, &rows, false)));
    }
    if super::sidebar::menu_item(
        ui,
        Icon::Rows,
        &format!("Copy {count}{noun} with column names"),
    ) {
        actions.push(Action::Copy(copy_rows(set, edits, &rows, true)));
    }
}

/// What a cell shows: a change not saved yet, else the server's value.
fn shown<'a>(
    set: &'a ResultSet,
    edits: &'a BTreeMap<(usize, usize), Option<String>>,
    row: usize,
    column: usize,
) -> Option<&'a str> {
    match edits.get(&(row, column)) {
        Some(change) => change.as_deref(),
        None => set.rows.get(row, column).flatten(),
    }
}

/// The picked cells as text to paste. One value is copied as it is; more
/// are tab-separated, a line per row. With `headers`, they are laid out as
/// a block of the picked columns under their names, cells not picked left
/// empty.
fn copy_selection(
    set: &ResultSet,
    edits: &BTreeMap<(usize, usize), Option<String>>,
    selected: &Selection,
    headers: bool,
) -> String {
    let mut out = String::new();
    if headers {
        let columns = selected.columns();
        write_row(
            &mut out,
            columns.iter().map(|c| Some(set.columns[*c].name.as_str())),
            '\t',
        );
        for row in selected.rows() {
            out.push('\n');
            let cells = columns.iter().map(|column| {
                if selected.contains((row, *column)) {
                    shown(set, edits, row, *column)
                } else {
                    None
                }
            });
            write_row(&mut out, cells, '\t');
        }
        return out;
    }
    if selected.len() == 1
        && let Some((row, column)) = selected.cells().next()
    {
        return shown(set, edits, row, column)
            .unwrap_or_default()
            .to_owned();
    }
    let mut current = None;
    let mut line = Vec::new();
    let flush = |out: &mut String, line: &mut Vec<Option<&str>>| {
        if !out.is_empty() {
            out.push('\n');
        }
        write_row(out, line.drain(..), '\t');
    };
    for (row, column) in selected.cells() {
        if current.is_some_and(|current| current != row) {
            flush(&mut out, &mut line);
        }
        current = Some(row);
        line.push(shown(set, edits, row, column));
    }
    if current.is_some() {
        flush(&mut out, &mut line);
    }
    out
}

/// Whole rows, tab-separated, optionally under the column names.
fn copy_rows(
    set: &ResultSet,
    edits: &BTreeMap<(usize, usize), Option<String>>,
    rows: &[usize],
    headers: bool,
) -> String {
    let mut out = String::new();
    if headers {
        write_row(
            &mut out,
            set.columns.iter().map(|c| Some(c.name.as_str())),
            '\t',
        );
    }
    for row in rows {
        if !out.is_empty() {
            out.push('\n');
        }
        let cells = (0..set.columns.len()).map(|column| shown(set, edits, *row, column));
        write_row(&mut out, cells, '\t');
    }
    out
}

/// Whether a grid's columns can be sorted, and by which one now: its
/// index and whether descending.
#[derive(Clone, Copy)]
enum Sorting {
    Off,
    On(Option<(usize, bool)>),
}

/// Changes waiting to be saved, or why the rows cannot change.
fn edit_bar(ui: &mut Ui, tab: u64, changes: usize, editing: &Editing, actions: &mut Vec<Action>) {
    let palette = Palette::current(ui.ctx());
    ui.add_space(8.0);
    if changes > 0 {
        widgets::pill(
            ui,
            &format!(
                "{changes} {} changed",
                if changes == 1 { "cell" } else { "cells" }
            ),
            palette.warning,
            palette.warning.gamma_multiply(0.15),
        );
        if matches!(editing, Editing::On(_))
            && widgets::button(ui, Tone::Primary, Some(Icon::Save), "Save…")
                .on_hover_text("Review the statements, then apply them")
                .clicked()
        {
            actions.push(Action::ReviewEdits(tab));
        }
        if widgets::button(ui, Tone::Ghost, None, "Discard").clicked() {
            actions.push(Action::DiscardEdits(tab));
        }
        return;
    }
    match editing {
        Editing::On(_) => {
            ui.label(RichText::new("Double-click a cell to edit").color(palette.dim));
        }
        Editing::Off(reason) if !reason.is_empty() => {
            widgets::pill(ui, "Read-only", palette.dim, palette.surface_hover)
                .on_hover_text(reason);
        }
        Editing::Off(_) | Editing::Loading => {}
    }
}

/// The first line, at most [`CELL_PREVIEW`] characters. Borrowed in the
/// common case, so drawing a cell allocates nothing.
fn preview(text: &str) -> Cow<'_, str> {
    let line = text.lines().next().unwrap_or_default();
    let end = line
        .char_indices()
        .nth(CELL_PREVIEW)
        .map_or(line.len(), |(at, _)| at);
    if end == text.len() {
        Cow::Borrowed(text)
    } else {
        Cow::Owned(format!("{}…", &line[..end]))
    }
}

/// A starting width from the header and the first rows' lengths.
fn column_width(set: &ResultSet, index: usize, name: &str) -> f32 {
    let longest = set
        .rows
        .iter()
        .take(64)
        .filter_map(|row| row.get(index).flatten())
        .map(|value| value.lines().next().unwrap_or_default().chars().count())
        .max()
        .unwrap_or(4)
        .max(name.chars().count() + 3);
    (longest as f32 * 7.6 + 28.0).clamp(72.0, 380.0)
}

fn error_card(ui: &mut Ui, error: &DbError, executed: &Executed, editor: &str) {
    let palette = Palette::current(ui.ctx());
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            egui::Frame::new()
                .outer_margin(egui::Margin::same(14))
                .inner_margin(egui::Margin::symmetric(16, 14))
                .fill(
                    palette
                        .danger
                        .gamma_multiply(if palette.dark { 0.08 } else { 0.06 }),
                )
                .stroke(Stroke::new(1.0, palette.danger.gamma_multiply(0.35)))
                .corner_radius(CornerRadius::same(10))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        let (rect, _) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::hover());
                        widgets::paint_icon(ui, Icon::CircleAlert, rect, palette.danger);
                        let title = match error.kind {
                            violet_core::ErrorKind::Cancelled => "Query cancelled",
                            violet_core::ErrorKind::NotConnected => "Not connected",
                            violet_core::ErrorKind::Connection => "Connection error",
                            _ => "Query failed",
                        };
                        ui.label(
                            RichText::new(title)
                                .font(theme::semibold(14.0))
                                .color(palette.danger),
                        );
                        if let Some(code) = &error.code {
                            widgets::pill(ui, code, palette.secondary, palette.surface_hover);
                        }
                    });
                    ui.add_space(6.0);
                    ui.add(
                        egui::Label::new(
                            RichText::new(&error.message)
                                .font(theme::mono(13.0))
                                .color(palette.text),
                        )
                        .wrap(),
                    );
                    if let Some(position) = error.position {
                        let editor_position = editor
                            .get(..executed.offset)
                            .map_or(0, |before| before.chars().count());
                        let (line, column) =
                            sql::line_column(editor, editor_position + position as usize);
                        ui.add_space(4.0);
                        ui.label(
                            RichText::new(format!("at line {line}, column {column}"))
                                .color(palette.secondary),
                        );
                    }
                    for (label, value) in [("Detail", &error.detail), ("Hint", &error.hint)] {
                        if let Some(value) = value {
                            ui.add_space(6.0);
                            ui.horizontal_wrapped(|ui| {
                                ui.label(
                                    RichText::new(label)
                                        .font(theme::semibold(12.5))
                                        .color(palette.secondary),
                                );
                                ui.label(RichText::new(value).color(palette.text));
                            });
                        }
                    }
                });
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ResultSet {
        let column = |name: &str| violet_core::ColumnMeta {
            name: name.into(),
            type_name: None,
            numeric: false,
        };
        let mut set = ResultSet {
            columns: vec![column("a"), column("b"), column("c")],
            ..ResultSet::default()
        };
        set.rows.push([Some("a0"), Some("b0"), Some("c0")]);
        set.rows.push([Some("a1"), None, Some("c1")]);
        set.rows.push([Some("a2"), Some("b2"), Some("c2")]);
        set
    }

    #[test]
    fn copies_what_is_picked() {
        let set = sample();
        let mut edits = BTreeMap::new();
        let mut selected = Selection::default();
        selected.click((0, 1), Mode::Replace);
        assert_eq!(copy_selection(&set, &edits, &selected, false), "b0");
        selected.click((1, 2), Mode::Extend);
        assert_eq!(
            copy_selection(&set, &edits, &selected, false),
            "b0\tc0\n\tc1"
        );
        selected.click((2, 0), Mode::Toggle);
        edits.insert((2, 0), Some("new".to_owned()));
        assert_eq!(
            copy_selection(&set, &edits, &selected, true),
            "a\tb\tc\n\tb0\tc0\n\t\tc1\nnew\t\t"
        );
    }

    #[test]
    fn copies_whole_rows() {
        let set = sample();
        let edits = BTreeMap::new();
        assert_eq!(
            copy_rows(&set, &edits, &[0, 2], false),
            "a0\tb0\tc0\na2\tb2\tc2"
        );
        assert_eq!(copy_rows(&set, &edits, &[1], true), "a\tb\tc\na1\t\tc1");
    }

    #[test]
    fn previews_show_the_first_line() {
        assert_eq!(preview("one\ntwo"), "one…");
        assert_eq!(preview("short"), "short");
        assert!(matches!(preview("short"), Cow::Borrowed(_)));
        let long = "x".repeat(CELL_PREVIEW + 5);
        assert_eq!(preview(&long).chars().count(), CELL_PREVIEW + 1);
    }
}
