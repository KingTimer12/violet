//! The results pane: a grid per result set, command summaries, errors, and
//! the progress of a running query.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

use egui::{Align, CornerRadius, Layout, RichText, Sense, Stroke, Ui, Vec2};
use egui_extras::{Column, TableBuilder};
use violet_core::{DbError, ResultSet};

use super::action::Action;
use super::cells::{self, Cell, Look};
use super::edit_flow::{EditTarget, Editing};
use super::icons::Icon;
use super::reference::{self, Reference};
use super::selection::{Mode, Selection};
use super::theme::{self, Palette};
use super::widgets::{self, Tone};
use super::workspace::{
    Count, Executed, InlineCell, Outcome, Paging, QueryTab, TabKind, format_duration, thousands,
};
use crate::export::{to_delimited, write_row};
use crate::sql;

const ROW_HEIGHT: f32 = 26.0;

#[hotpath::measure]
pub fn show(
    ui: &mut Ui,
    tab: &mut QueryTab,
    editing: &Editing,
    links: &[bool],
    reference: Option<&Reference>,
    fetch_size: u64,
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
            let hint = ui.ctx().format_shortcut(&crate::ui::shortcuts::get(
                crate::ui::shortcuts::Command::RunStatement,
            ));
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
            let server_rows = set.rows.len();
            let pending = Pending {
                cells: tab
                    .edits
                    .keys()
                    .filter(|(row, _)| *row < server_rows && !tab.deleted.contains(row))
                    .count(),
                added: tab.added,
                deleted: tab.deleted.len(),
                picked: tab
                    .grid_selection
                    .rows()
                    .into_iter()
                    .partition(|row| tab.deleted.contains(row)),
            };

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
                            edit_bar(ui, tab.id, &pending, editing, actions);
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
                        .show(ui, |ui| paging_bar(ui, tab.id, paging, fetching, fetch_size, actions));
                }
                let sort = match &tab.kind {
                    // Redis returns a key's elements in its own order.
                    TabKind::Data(view) if view.subtype.is_some() => Sorting::Off,
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
                let changes = GridChanges {
                    target: match editing {
                        Editing::On(target) => Some(target),
                        Editing::Off(_) | Editing::Loading => None,
                    },
                    edits: &tab.edits,
                    added: tab.added,
                    deleted: &tab.deleted,
                };
                let reveal = tab.reveal_row.take();
                // Scrolling to the end reads the next page, as DBeaver does.
                let more = tab.paging.as_ref().is_some_and(|paging| paging.has_more)
                    && tab.running.is_none()
                    && tab.added == 0;
                grid(
                    ui,
                    tab.id,
                    set,
                    sort,
                    &mut tab.grid_selection,
                    &changes,
                    &mut tab.inline,
                    reveal,
                    more,
                    links,
                    reference,
                    actions,
                );
            } else if let (Some(command), None) = (&set.command, set.affected) {
                // A reply such as Redis's `OK`.
                widgets::empty_state(ui, Icon::CircleCheck, command, "The command ran.");
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
fn paging_bar(
    ui: &mut Ui,
    tab: u64,
    paging: &Paging,
    fetching: bool,
    fetch_size: u64,
    actions: &mut Vec<Action>,
) {
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
            let next = format!("Next {}", thousands(usize::try_from(fetch_size).unwrap_or(usize::MAX)));
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
            fetch_size_picker(ui, tab, fetch_size, actions);
        });
    });
}

const FETCH_SIZES: [u64; 6] = [50, 100, 200, 500, 1_000, 5_000];

/// The rows a page reads: a preset, or any number typed in.
fn fetch_size_picker(ui: &mut Ui, tab: u64, fetch_size: u64, actions: &mut Vec<Action>) {
    let mut size = fetch_size;
    egui::ComboBox::from_id_salt(("fetch-size", tab))
        .selected_text(thousands(usize::try_from(size).unwrap_or(usize::MAX)))
        .width(70.0)
        // Clicking the custom field would close the list before typing.
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show_ui(ui, |ui| {
            for preset in FETCH_SIZES {
                if ui
                    .selectable_value(
                        &mut size,
                        preset,
                        thousands(usize::try_from(preset).unwrap_or(usize::MAX)),
                    )
                    .clicked()
                {
                    ui.close();
                }
            }
            ui.separator();
            ui.horizontal(|ui| {
                ui.label("Custom");
                ui.add(egui::DragValue::new(&mut size).range(1..=100_000));
            });
        })
        .response
        .on_hover_text("Rows read at a time");
    if size != fetch_size {
        actions.push(Action::SetFetchSize(size));
    }
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
    changes: &GridChanges<'_>,
    inline: &mut Option<InlineCell>,
    reveal: Option<usize>,
    more: bool,
    links: &[bool],
    reference: Option<&Reference>,
    actions: &mut Vec<Action>,
) {
    let palette = Palette::current(ui.ctx());
    let (target, edits) = (changes.target, changes.edits);
    // The result's rows, then the added ones.
    let rows = set.rows.len() + changes.added;
    // Added rows set the key too; rows to delete change no more.
    let can_edit = |row: usize, column: usize| {
        !changes.deleted.contains(&row)
            && target.is_some_and(|t| {
                let columns = if row >= set.rows.len() {
                    &t.insertable
                } else {
                    &t.editable
                };
                columns.get(column).copied().unwrap_or(false)
            })
    };
    let number_width = 14.0 + 7.5 * rows.max(1).to_string().len() as f32;

    // Clicks pick cells as DBeaver does: Ctrl (⌘) adds, Shift extends.
    let mode = Mode::from_modifiers(ui.input(|input| input.modifiers));
    let columns = set.columns.len();
    let looks: Vec<Look> = set.columns.iter().map(Look::of).collect();

    // The platform shortcuts copy the picked cells and pick them all,
    // once the grid is in use and no text field takes the keys.
    if !selected.is_empty() && !ui.ctx().egui_wants_keyboard_input() {
        let copy = ui.input(|input| {
            input
                .events
                .iter()
                .any(|event| matches!(event, egui::Event::Copy))
        });
        // Ctrl+Shift+C also arrives as a copy: it opens advanced copy.
        if copy && ui.input(|input| input.modifiers.shift) {
            actions.push(Action::AdvancedCopy(tab));
        } else if copy {
            actions.push(Action::Copy(copy_selection(set, edits, selected, false)));
        }
        if ui.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::A)) {
            selected.select_all(rows, columns);
        }
        if target.is_some_and(EditTarget::rows_change)
            && ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Delete))
        {
            actions.push(Action::DeleteRows(tab, selected.rows()));
        }
        // Enter or F2 edits the cell clicked last, in place.
        let edit_key = ui.input_mut(|input| {
            input.consume_key(egui::Modifiers::NONE, egui::Key::Enter)
                || input.consume_key(egui::Modifiers::NONE, egui::Key::F2)
        });
        if edit_key
            && inline.is_none()
            && let Some((row, column)) = selected.focus
            && can_edit(row, column)
        {
            start_edit(tab, set, edits, row, column, inline, actions);
        }
    }

    // The rows a followed foreign key points at open in an extra row under
    // the one it was followed from: grid rows past it move down one.
    let opened = reference.filter(|reference| reference.row < set.rows.len());
    let shown = |index: usize| match opened {
        Some(reference) if index > reference.row => index + 1,
        _ => index,
    };
    let extra = opened.map(|reference| reference.row + 1);

    egui::ScrollArea::horizontal()
        .id_salt(("grid-h", tab))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            // What the grid shows across: the extra row spans it.
            let viewport = ui.clip_rect();
            let mut table = TableBuilder::new(ui)
                .id_salt(("grid", tab))
                .striped(true)
                .resizable(true)
                .cell_layout(Layout::left_to_right(Align::Center))
                .auto_shrink([false, false])
                .min_scrolled_height(0.0)
                .max_scroll_height(f32::INFINITY)
                .column(Column::exact(number_width));
            if let Some(row) = reveal {
                table = table.scroll_to_row(shown(row), Some(Align::BOTTOM));
            }
            for (index, column) in set.columns.iter().enumerate() {
                table = table.column(
                    Column::initial(column_width(set, index, &column.name))
                        .at_least(48.0)
                        .clip(true),
                );
            }
            let output = table
                .header(30.0, |mut header| {
                    header.col(|ui| {
                        ui.painter().rect_filled(ui.max_rect(), 0.0, palette.panel);
                    });
                    for (index, column) in set.columns.iter().enumerate() {
                        let mut head = None;
                        header.col(|ui| {
                            ui.painter().rect_filled(ui.max_rect(), 0.0, palette.panel);
                            ui.add_space(6.0);
                            // A foreign key: its values open the row they
                            // point at.
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
                    let heights =
                        (0..rows + usize::from(extra.is_some())).map(|index| match opened {
                            Some(reference) if Some(index) == extra => reference::height(reference),
                            _ => ROW_HEIGHT,
                        });
                    body.heterogeneous_rows(heights, |mut row| {
                        if let Some(reference) = opened
                            && Some(row.index()) == extra
                        {
                            // Every column, the last one painting the row
                            // across them all.
                            for column in 0..=columns {
                                row.col(|ui| {
                                    if column < columns {
                                        return;
                                    }
                                    let cell = ui.max_rect();
                                    let clip = egui::Rect::from_x_y_ranges(
                                        viewport.x_range(),
                                        ui.clip_rect().y_range(),
                                    );
                                    let rect = egui::Rect::from_x_y_ranges(
                                        viewport.x_range(),
                                        cell.y_range(),
                                    );
                                    reference::show(ui, rect, clip, reference, actions);
                                });
                            }
                            return;
                        }
                        let index = match extra {
                            Some(extra) if row.index() > extra => row.index() - 1,
                            _ => row.index(),
                        };
                        let added = index >= set.rows.len();
                        let deleted = changes.deleted.contains(&index);
                        // Added rows in green, rows to delete in red.
                        let tint = if added {
                            Some(palette.success.gamma_multiply(0.15))
                        } else if deleted {
                            Some(palette.danger.gamma_multiply(0.18))
                        } else {
                            None
                        };
                        row.set_selected(selected.has_row(index));
                        // The row number picks the whole row.
                        let mut number = None;
                        row.col(|ui| {
                            if let Some(tint) = tint {
                                ui.painter().rect_filled(ui.max_rect(), 0.0, tint);
                            }
                            cells::paint_text(ui, Cell::RowNumber(index), true, palette.dim);
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
                                if target.is_some_and(EditTarget::rows_change) {
                                    change_menu(ui, tab, changes.deleted, selected, actions);
                                    ui.separator();
                                }
                                rows_menu(ui, set, edits, selected, actions);
                                advanced_copy_item(ui, tab, actions);
                            });
                        }
                        for (column, meta) in set.columns.iter().enumerate() {
                            let edited = edits.get(&(index, column));
                            // A change not saved yet shows instead of the
                            // server's value.
                            let value = match edited {
                                Some(change) => change.as_deref(),
                                None => set.rows.get(index, column).flatten(),
                            };
                            let editable = can_edit(index, column);
                            let mut cell = None;
                            // A foreign key value's link.
                            let mut chip = None;
                            // Set when the field in the cell lets go of
                            // the keyboard: whether by Escape, by Tab, and
                            // with Shift.
                            let mut finished = None;
                            row.col(|ui| {
                                if let Some(tint) = tint {
                                    ui.painter().rect_filled(ui.max_rect(), 0.0, tint);
                                }
                                if let Some(edit) = inline
                                    .as_mut()
                                    .filter(|edit| (edit.row, edit.column) == (index, column))
                                {
                                    let response = ui.add_sized(
                                        ui.max_rect().size(),
                                        egui::TextEdit::singleline(&mut edit.text)
                                            .font(theme::mono(12.5))
                                            .margin(egui::Margin::symmetric(5, 2)),
                                    );
                                    if std::mem::take(&mut edit.focus) {
                                        response.request_focus();
                                    }
                                    if response.lost_focus() {
                                        finished = Some(ui.input(|input| {
                                            (
                                                input.key_pressed(egui::Key::Escape),
                                                input.key_pressed(egui::Key::Tab),
                                                input.modifiers.shift,
                                            )
                                        }));
                                    }
                                    return;
                                }
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
                                    None if added && edited.is_none() => {
                                        (Cell::Default, palette.dim)
                                    }
                                    None => (Cell::Null, palette.dim),
                                    Some(text) if deleted => (Cell::Value(text), palette.dim),
                                    Some(text) => (Cell::Value(text), palette.text),
                                };
                                // Rows not saved yet point at nothing.
                                let link = links.get(column).copied().unwrap_or(false)
                                    && !added
                                    && !deleted;
                                chip = cells::paint(
                                    ui,
                                    cell_text,
                                    looks[column],
                                    meta.numeric,
                                    color,
                                    link,
                                )
                                .link;
                                // The cell is painted, not laid out, and the
                                // table's own response for it has no area:
                                // it takes clicks itself.
                                cell = Some(ui.interact(
                                    ui.max_rect(),
                                    ui.id().with("cell"),
                                    Sense::click(),
                                ));
                            });
                            if let Some((escape, tab_key, back)) = finished
                                && let Some(edit) = inline.take()
                            {
                                if !escape && edit.text != edit.initial {
                                    actions.push(Action::SetCell(
                                        tab,
                                        edit.row,
                                        edit.column,
                                        Some(edit.text),
                                    ));
                                }
                                // Tab goes on to the next cell of the row
                                // that can change, Shift+Tab back.
                                let next = if back {
                                    (0..column).rev().find(|c| can_edit(index, *c))
                                } else {
                                    (column + 1..columns).find(|c| can_edit(index, *c))
                                };
                                if tab_key
                                    && !escape
                                    && let Some(next) = next
                                {
                                    selected.click((index, next), Mode::Replace);
                                    start_edit(tab, set, edits, index, next, inline, actions);
                                }
                            }
                            let Some(response) = cell else {
                                continue;
                            };
                            if response.clicked() {
                                selected.click((index, column), mode);
                                let on_link = response
                                    .interact_pointer_pos()
                                    .zip(chip)
                                    .is_some_and(|(pointer, chip)| chip.contains(pointer));
                                if on_link && mode == Mode::Replace {
                                    actions.push(Action::FollowKey(tab, index, column));
                                }
                            }
                            if chip.is_some_and(|chip| {
                                response.hover_pos().is_some_and(|p| chip.contains(p))
                            }) {
                                response.ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
                            }
                            // Right-clicking outside the selection picks the
                            // cell; inside, the menu acts on all of it.
                            if response.secondary_clicked() && !selected.contains((index, column)) {
                                selected.click((index, column), Mode::Replace);
                            }
                            if response.double_clicked() && editable {
                                start_edit(tab, set, edits, index, column, inline, actions);
                            }
                            // Long values get a tooltip; only measured when hovered.
                            if response.hovered()
                                && let Some(text) = value
                            {
                                let text = match looks[column] {
                                    Look::Geometry => cells::geometry_text(text),
                                    Look::Text | Look::Boolean => Cow::Borrowed(text),
                                };
                                if text.len() > 40 || text.contains('\n') {
                                    response.clone().on_hover_ui(|ui| {
                                        ui.set_max_width(480.0);
                                        ui.label(RichText::new(text).font(theme::mono(12.0)));
                                    });
                                }
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
                                    let revert = if added {
                                        "Use the default"
                                    } else {
                                        "Revert change"
                                    };
                                    if edited.is_some()
                                        && super::sidebar::menu_item(ui, Icon::Refresh, revert)
                                    {
                                        actions.push(Action::RevertCell(tab, index, column));
                                    }
                                    ui.separator();
                                }
                                if target.is_some_and(EditTarget::rows_change) {
                                    change_menu(ui, tab, changes.deleted, selected, actions);
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
                                if looks[column] == Look::Geometry
                                    && let Some(text) = value
                                    && let Some(wkt) = crate::geometry::wkt(text)
                                    && super::sidebar::menu_item(ui, Icon::Copy, "Copy as WKT")
                                {
                                    actions.push(Action::Copy(wkt));
                                }
                                if chip.is_some()
                                    && super::sidebar::menu_item(
                                        ui,
                                        Icon::ArrowUpRight,
                                        "Open referenced row",
                                    )
                                {
                                    actions.push(Action::FollowKey(tab, index, column));
                                }
                                advanced_copy_item(ui, tab, actions);
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
            let bottom = output.state.offset.y + output.inner_rect.height();
            if more && bottom >= output.content_size.y - 4.0 * ROW_HEIGHT {
                actions.push(Action::FetchMore(tab));
            }
        });
}

/// Edits a cell in place, or in the dialog when its value has several
/// lines. A NULL or default cell starts empty.
fn start_edit(
    tab: u64,
    set: &ResultSet,
    edits: &BTreeMap<(usize, usize), Option<String>>,
    row: usize,
    column: usize,
    inline: &mut Option<InlineCell>,
    actions: &mut Vec<Action>,
) {
    let text = shown(set, edits, row, column).unwrap_or_default();
    if text.contains('\n') {
        actions.push(Action::EditCell(tab, row, column));
    } else {
        *inline = Some(InlineCell::new(row, column, text));
    }
}

/// What the grid shows of its changes not saved yet, and whether its rows
/// can change at all.
struct GridChanges<'a> {
    target: Option<&'a EditTarget>,
    edits: &'a BTreeMap<(usize, usize), Option<String>>,
    added: usize,
    deleted: &'a BTreeSet<usize>,
}

/// Adding a row, and deleting the picked rows or keeping them after all.
fn change_menu(
    ui: &mut Ui,
    tab: u64,
    deleted: &BTreeSet<usize>,
    selected: &Selection,
    actions: &mut Vec<Action>,
) {
    if super::sidebar::menu_item(ui, Icon::Plus, "Add row") {
        actions.push(Action::AddRow(tab));
    }
    let (restore, delete): (Vec<usize>, Vec<usize>) = selected
        .rows()
        .into_iter()
        .partition(|row| deleted.contains(row));
    let label = |verb: &str, count: usize| match count {
        1 => format!("{verb} row"),
        count => format!("{verb} {} rows", thousands(count)),
    };
    if !delete.is_empty() {
        let shortcut = ui.ctx().format_shortcut(&egui::KeyboardShortcut::new(
            egui::Modifiers::NONE,
            egui::Key::Delete,
        ));
        let label = format!("{}   {shortcut}", label("Delete", delete.len()));
        if super::sidebar::menu_item(ui, Icon::Trash, &label) {
            actions.push(Action::DeleteRows(tab, delete));
        }
    }
    if !restore.is_empty()
        && super::sidebar::menu_item(ui, Icon::Refresh, &label("Keep", restore.len()))
    {
        actions.push(Action::RestoreRows(tab, restore));
    }
}

/// Opens advanced copy, with its shortcut shown.
fn advanced_copy_item(ui: &mut Ui, tab: u64, actions: &mut Vec<Action>) {
    let shortcut = ui.ctx().format_shortcut(&egui::KeyboardShortcut::new(
        egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
        egui::Key::C,
    ));
    if super::sidebar::menu_item(ui, Icon::Copy, &format!("Advanced copy…   {shortcut}")) {
        actions.push(Action::AdvancedCopy(tab));
    }
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

/// A grid's changes not saved yet.
struct Pending {
    /// Cells changed in rows of the result that are kept.
    cells: usize,
    added: usize,
    deleted: usize,
    /// The picked rows: those marked to delete, and the others.
    picked: (Vec<usize>, Vec<usize>),
}

/// Changes waiting to be saved, or why the rows cannot change.
fn edit_bar(
    ui: &mut Ui,
    tab: u64,
    pending: &Pending,
    editing: &Editing,
    actions: &mut Vec<Action>,
) {
    let palette = Palette::current(ui.ctx());
    ui.add_space(8.0);
    let rows_change = matches!(editing, Editing::On(target) if target.rows_change());
    if rows_change
        && widgets::button(ui, Tone::Ghost, Some(Icon::Plus), "Add row")
            .on_hover_text("Add a row, inserted when you save the changes")
            .clicked()
    {
        actions.push(Action::AddRow(tab));
    }
    // Deletes the picked rows; picked rows all marked keeps them instead.
    if rows_change {
        let (marked, kept) = &pending.picked;
        let count = |verb: &str, rows: &[usize]| match rows.len() {
            0 | 1 => format!("{verb} row"),
            count => format!("{verb} {} rows", thousands(count)),
        };
        if kept.is_empty() && !marked.is_empty() {
            if widgets::button(ui, Tone::Ghost, Some(Icon::Refresh), &count("Keep", marked))
                .on_hover_text("Keep the picked rows, not deleting them")
                .clicked()
            {
                actions.push(Action::RestoreRows(tab, marked.clone()));
            }
        } else {
            let response = ui
                .add_enabled_ui(!kept.is_empty(), |ui| {
                    widgets::button(ui, Tone::Ghost, Some(Icon::Trash), &count("Delete", kept))
                })
                .inner
                .on_hover_text("Delete the picked rows when you save the changes (Delete)")
                .on_disabled_hover_text("Pick the rows to delete");
            if response.clicked() {
                actions.push(Action::DeleteRows(tab, kept.clone()));
            }
        }
    }
    let mut parts = Vec::new();
    if pending.cells > 0 {
        let noun = if pending.cells == 1 { "cell" } else { "cells" };
        parts.push(format!("{} {noun} changed", pending.cells));
    }
    if pending.added > 0 {
        parts.push(format!("{} added", pending.added));
    }
    if pending.deleted > 0 {
        parts.push(format!("{} to delete", pending.deleted));
    }
    if !parts.is_empty() {
        widgets::pill(
            ui,
            &parts.join(" · "),
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

/// A starting width from the header and the first rows' lengths.
pub(super) fn column_width(set: &ResultSet, index: usize, name: &str) -> f32 {
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
}
