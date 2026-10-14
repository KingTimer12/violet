//! The query history: every query run from an editor, searchable, to run
//! again or bring back into an editor.

use egui::{Align2, CornerRadius, Key, Modifiers, RichText, Sense, Stroke, Vec2};
use violet_core::ConnectionId;
use violet_core::ports::HistoryEntry;

use super::action::Action;
use super::editor;
use super::icons::Icon;
use super::theme::{self, Palette};
use super::widgets::{self, Tone};
use super::workspace::{format_duration, thousands};

const ROW_HEIGHT: f32 = 52.0;

/// The dialog's state while it is open.
#[derive(Debug, Default)]
pub struct HistoryView {
    pub search: String,
    /// Only the active editor's connection.
    pub this_connection: bool,
    /// The selected entry, by its index in the history.
    pub selected: Option<usize>,
    pub confirm_clear: bool,
    /// Keyboard moved the selection: scroll it into view.
    reveal: bool,
    scroll: f32,
    viewport: f32,
}

/// Where the dialog sends what it picks.
pub struct Target {
    /// The active SQL editor, to insert into.
    pub editor: bool,
    pub connection: Option<ConnectionId>,
}

/// Draws the dialog. Returns `false` once it should close.
pub fn show(
    ctx: &egui::Context,
    view: &mut HistoryView,
    entries: &[HistoryEntry],
    target: &Target,
    actions: &mut Vec<Action>,
) -> bool {
    let palette = Palette::current(ctx);
    let mut open = true;
    let before = actions.len();

    let needle = view.search.trim().to_lowercase();
    let only = target.connection.filter(|_| view.this_connection);
    // Newest first.
    let shown: Vec<usize> = (0..entries.len())
        .rev()
        .filter(|&index| {
            let entry = &entries[index];
            only.is_none_or(|id| entry.connection == Some(id))
                && (needle.is_empty()
                    || entry.sql.to_lowercase().contains(&needle)
                    || entry.source.to_lowercase().contains(&needle))
        })
        .collect();
    let position = view
        .selected
        .and_then(|selected| shown.iter().position(|&index| index == selected));
    if position.is_none() {
        view.selected = shown.first().copied();
    }

    // Keys: ↑↓ choose, ↵ uses the entry.
    let mut use_selected = false;
    ctx.input_mut(|input| {
        let position = view
            .selected
            .and_then(|selected| shown.iter().position(|&index| index == selected));
        if let Some(position) = position {
            if input.consume_key(Modifiers::NONE, Key::ArrowDown) && position + 1 < shown.len() {
                view.selected = Some(shown[position + 1]);
                view.reveal = true;
            }
            if input.consume_key(Modifiers::NONE, Key::ArrowUp) && position > 0 {
                view.selected = Some(shown[position - 1]);
                view.reveal = true;
            }
        }
        use_selected = input.consume_key(Modifiers::NONE, Key::Enter);
    });

    let modal = egui::Modal::new(egui::Id::new("query-history"))
        .frame(
            egui::Frame::new()
                .fill(palette.surface)
                .stroke(Stroke::new(1.0, palette.outline))
                .corner_radius(CornerRadius::same(14))
                .shadow(ctx.global_style().visuals.window_shadow)
                .inner_margin(egui::Margin::same(22)),
        )
        .show(ctx, |ui| {
            ui.set_width(780.0);
            ui.spacing_mut().item_spacing.y = 12.0;
            header(ui, entries.len(), &mut open, &palette);

            ui.horizontal(|ui| {
                let (icon, _) = ui.allocate_exact_size(Vec2::splat(16.0), Sense::hover());
                widgets::paint_icon(ui, Icon::Search, icon, palette.dim);
                let search = ui.add(
                    widgets::text_input(&mut view.search, "Search queries and connections")
                        .desired_width(420.0),
                );
                if !ui.ctx().memory(|m| m.focused().is_some()) {
                    search.request_focus();
                }
                if target.connection.is_some() {
                    widgets::segmented(
                        ui,
                        &mut view.this_connection,
                        &[(false, "All"), (true, "This connection")],
                    );
                }
            });

            if shown.is_empty() {
                ui.add_space(30.0);
                let body = if entries.is_empty() {
                    "Queries you run from an editor show up here."
                } else {
                    "No query matches."
                };
                ui.allocate_ui(Vec2::new(ui.available_width(), 160.0), |ui| {
                    widgets::empty_state(ui, Icon::Clock, "No queries", body);
                });
            } else {
                list(ui, view, entries, &shown, actions, target, &palette);
                if let Some(entry) = view.selected.and_then(|index| entries.get(index)) {
                    preview(ui, entry, &palette);
                }
            }

            footer(ui, view, entries, target, use_selected, actions, &palette);
        });
    if modal.should_close() {
        open = false;
    }
    // Using a query closes the dialog.
    open && !actions[before..]
        .iter()
        .any(|action| matches!(action, Action::OpenEditor { .. } | Action::InsertSql(_)))
}

fn header(ui: &mut egui::Ui, count: usize, open: &mut bool, palette: &Palette) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(36.0), Sense::hover());
        ui.painter()
            .rect_filled(rect, CornerRadius::same(10), palette.accent_soft);
        widgets::paint_icon(ui, Icon::Clock, rect.shrink(9.0), palette.accent);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.label(
                RichText::new("Query history")
                    .font(theme::semibold(17.0))
                    .color(palette.text),
            );
            ui.label(
                RichText::new(format!(
                    "{} queries, kept on this machine only",
                    thousands(count)
                ))
                .color(palette.secondary),
            );
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
            if widgets::icon_button(ui, Icon::Close, "Close").clicked() {
                *open = false;
            }
        });
    });
}

fn list(
    ui: &mut egui::Ui,
    view: &mut HistoryView,
    entries: &[HistoryEntry],
    shown: &[usize],
    actions: &mut Vec<Action>,
    target: &Target,
    palette: &Palette,
) {
    let mut area = egui::ScrollArea::vertical()
        .id_salt("history-list")
        .max_height(ROW_HEIGHT * 6.0)
        .auto_shrink([false, true]);
    if std::mem::take(&mut view.reveal)
        && let Some(position) = view
            .selected
            .and_then(|selected| shown.iter().position(|&index| index == selected))
    {
        let top = position as f32 * ROW_HEIGHT;
        if top < view.scroll {
            area = area.vertical_scroll_offset(top);
        } else if top + ROW_HEIGHT > view.scroll + view.viewport {
            area = area.vertical_scroll_offset(top + ROW_HEIGHT - view.viewport);
        }
    }
    let now = jiff::Zoned::now();
    let output = area.show_rows(ui, ROW_HEIGHT, shown.len(), |ui, rows| {
        ui.spacing_mut().item_spacing.y = 0.0;
        for position in rows {
            let index = shown[position];
            let entry = &entries[index];
            let (rect, response) =
                ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_HEIGHT), Sense::click());
            let selected = view.selected == Some(index);
            row(ui, rect, entry, selected, response.hovered(), &now, palette);
            if response.clicked() {
                view.selected = Some(index);
            }
            if response.double_clicked() {
                actions.push(open_action(entry, target));
            }
        }
    });
    view.scroll = output.state.offset.y;
    view.viewport = output.inner_rect.height();
}

fn row(
    ui: &egui::Ui,
    rect: egui::Rect,
    entry: &HistoryEntry,
    selected: bool,
    hovered: bool,
    now: &jiff::Zoned,
    palette: &Palette,
) {
    let painter = ui.painter();
    if selected {
        painter.rect_filled(
            rect.shrink2(Vec2::new(0.0, 1.0)),
            CornerRadius::same(7),
            palette.accent_soft,
        );
    } else if hovered {
        painter.rect_filled(
            rect.shrink2(Vec2::new(0.0, 1.0)),
            CornerRadius::same(7),
            palette.surface_hover,
        );
    }
    let left = rect.left() + 14.0;
    let sql = entry.sql.split_whitespace().collect::<Vec<_>>().join(" ");
    let line = egui::Rect::from_min_max(
        egui::pos2(left, rect.top() + 7.0),
        egui::pos2(rect.right() - 12.0, rect.top() + 26.0),
    );
    widgets::paint_text(
        ui,
        line,
        Align2::LEFT_CENTER,
        &sql,
        theme::mono(12.5),
        if selected {
            palette.text
        } else {
            palette.secondary
        },
    );

    let meta_top = rect.top() + 27.0;
    let color = if entry.error.is_some() {
        palette.danger
    } else {
        palette.success
    };
    painter.circle_filled(egui::pos2(left + 3.0, meta_top + 9.0), 3.0, color);
    let mut meta = format!(
        "{}  ·  {}  ·  {}",
        when(entry.at, now),
        entry.source,
        format_duration(std::time::Duration::from_millis(entry.elapsed_ms))
    );
    match (&entry.error, entry.rows) {
        (Some(_), _) => {}
        (None, Some(rows)) => {
            meta.push_str(&format!(
                "  ·  {} {}",
                thousands(usize::try_from(rows).unwrap_or(usize::MAX)),
                if rows == 1 { "row" } else { "rows" }
            ));
        }
        (None, None) => {}
    }
    let meta_rect = egui::Rect::from_min_max(
        egui::pos2(left + 12.0, meta_top),
        egui::pos2(rect.right() - 12.0, meta_top + 18.0),
    );
    let galley = widgets::elided(
        ui,
        &meta,
        theme::medium(11.5),
        palette.dim,
        meta_rect.width(),
    );
    let used = galley.size().x;
    painter.galley(
        Align2::LEFT_CENTER
            .align_size_within_rect(galley.size(), meta_rect)
            .min,
        galley,
        palette.dim,
    );
    if let Some(error) = &entry.error {
        let error_rect = egui::Rect::from_min_max(
            egui::pos2(meta_rect.left() + used + 12.0, meta_rect.top()),
            meta_rect.right_bottom(),
        );
        if error_rect.width() > 40.0 {
            widgets::paint_text(
                ui,
                error_rect,
                Align2::LEFT_CENTER,
                error,
                theme::medium(11.5),
                palette.danger,
            );
        }
    }
}

fn preview(ui: &mut egui::Ui, entry: &HistoryEntry, palette: &Palette) {
    egui::Frame::new()
        .fill(palette.window)
        .stroke(Stroke::new(1.0, palette.outline))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            egui::ScrollArea::vertical()
                .id_salt("history-preview")
                .max_height(150.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    let mut job = editor::highlighted(&entry.sql, palette);
                    job.wrap.max_width = ui.available_width();
                    ui.add(egui::Label::new(job).selectable(true));
                });
        });
}

fn footer(
    ui: &mut egui::Ui,
    view: &mut HistoryView,
    entries: &[HistoryEntry],
    target: &Target,
    use_selected: bool,
    actions: &mut Vec<Action>,
    palette: &Palette,
) {
    let entry = view.selected.and_then(|index| entries.get(index));
    ui.horizontal(|ui| {
        if !entries.is_empty() {
            let label = if view.confirm_clear {
                "Click again to clear"
            } else {
                "Clear history"
            };
            let tone = if view.confirm_clear {
                Tone::Danger
            } else {
                Tone::Ghost
            };
            let response = widgets::button(ui, tone, Some(Icon::Trash), label);
            if response.clicked() {
                if view.confirm_clear {
                    actions.push(Action::ClearHistory);
                    view.selected = None;
                }
                view.confirm_clear = !view.confirm_clear;
            } else if !response.hovered() && ui.input(|i| i.pointer.any_click()) {
                view.confirm_clear = false;
            }
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let Some(entry) = entry else { return };
            if widgets::button(
                ui,
                Tone::Primary,
                Some(Icon::Terminal),
                "Open in new editor",
            )
            .clicked()
            {
                actions.push(Action::OpenEditor {
                    connection: entry.connection.or(target.connection),
                    sql: entry.sql.clone(),
                });
            }
            let insert = ui
                .add_enabled_ui(target.editor, |ui| {
                    widgets::button(
                        ui,
                        Tone::Secondary,
                        Some(Icon::FileCode),
                        "Insert at cursor",
                    )
                })
                .inner
                .on_disabled_hover_text("Open a SQL editor first");
            if insert.clicked() {
                actions.push(Action::InsertSql(entry.sql.clone()));
            }
            if widgets::icon_button(ui, Icon::Copy, "Copy").clicked() {
                actions.push(Action::Copy(entry.sql.clone()));
            }
            ui.label(
                RichText::new("↵ uses the selected query")
                    .size(11.0)
                    .color(palette.dim),
            );
        });
    });
    if use_selected && let Some(entry) = entry {
        actions.push(open_action(entry, target));
    }
}

/// Into the active editor when there is one, else a new editor.
fn open_action(entry: &HistoryEntry, target: &Target) -> Action {
    if target.editor {
        Action::InsertSql(entry.sql.clone())
    } else {
        Action::OpenEditor {
            connection: entry.connection.or(target.connection),
            sql: entry.sql.clone(),
        }
    }
}

/// `14:03` today, `Yesterday 14:03`, `Oct 12 14:03` this year, else the
/// full date; in the local time zone.
fn when(at: u64, now: &jiff::Zoned) -> String {
    let Ok(timestamp) = jiff::Timestamp::from_millisecond(i64::try_from(at).unwrap_or(i64::MAX))
    else {
        return String::new();
    };
    let time = timestamp.to_zoned(now.time_zone().clone());
    let (date, today) = (time.date(), now.date());
    if date == today {
        time.strftime("%H:%M").to_string()
    } else if today.yesterday().is_ok_and(|yesterday| yesterday == date) {
        time.strftime("Yesterday %H:%M").to_string()
    } else if date.year() == today.year() {
        time.strftime("%b %-d %H:%M").to_string()
    } else {
        time.strftime("%Y-%m-%d %H:%M").to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_read_relative_to_today() {
        let now: jiff::Zoned = "2026-10-14T15:30:00-03:00[America/Sao_Paulo]"
            .parse()
            .expect("time");
        let at = |text: &str| {
            let zoned: jiff::Zoned = text.parse().expect("time");
            u64::try_from(zoned.timestamp().as_millisecond()).expect("positive")
        };
        assert_eq!(
            when(at("2026-10-14T09:05:00-03:00[America/Sao_Paulo]"), &now),
            "09:05"
        );
        assert_eq!(
            when(at("2026-10-13T23:59:00-03:00[America/Sao_Paulo]"), &now),
            "Yesterday 23:59"
        );
        assert_eq!(
            when(at("2026-02-01T08:00:00-03:00[America/Sao_Paulo]"), &now),
            "Feb 1 08:00"
        );
        assert_eq!(
            when(at("2025-12-31T08:00:00-03:00[America/Sao_Paulo]"), &now),
            "2025-12-31 08:00"
        );
    }
}
