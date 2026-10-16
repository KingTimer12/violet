//! Advanced copy (Ctrl+Shift+C): the picked cells copied in any of the
//! export's text formats, with a preview of what lands on the clipboard.

use std::collections::BTreeMap;

use egui::{RichText, Stroke};
use violet_core::{Dialect as _, ResultSet};

use super::VioletApp;
use super::action::Action;
use super::icons::Icon;
use super::selection::Selection;
use super::theme::{self, Palette};
use super::widgets::{self, Tone};
use super::workspace::{Outcome, TabId, thousands};
use crate::export::{self, Format};

/// Characters the preview shows.
const PREVIEW: usize = 6_000;

pub struct CopyDialog {
    /// The picked cells, as a result of their own: the picked rows and
    /// columns, cells not picked as NULL.
    set: ResultSet,
    table: String,
    title: String,
    dialect: violet_core::SqlDialect,
    pub format: Format,
    pub headers: bool,
    /// The text for the format and headers it was made for.
    text: Option<(Format, bool, Result<String, String>)>,
}

impl VioletApp {
    /// Opens advanced copy for what is picked in `tab_id`'s grid, or all
    /// of its rows when nothing is.
    pub(super) fn open_copy_dialog(&mut self, tab_id: TabId) {
        let Some(tab) = self.workspace.get(tab_id) else {
            return;
        };
        let Some(Outcome::Done { output }) = &tab.outcome else {
            return;
        };
        let Some(set) = output.sets.get(tab.active_set) else {
            return;
        };
        if !set.has_rows() {
            return;
        }
        let picked = picked(set, &tab.edits, &tab.grid_selection);
        let (table, title, dialect) = self.export_target(tab);
        let (format, headers) = self.copy_choice;
        self.copy_dialog = Some(CopyDialog {
            set: picked,
            table,
            title,
            dialect,
            format,
            headers,
            text: None,
        });
    }
}

/// The picked cells as a result set of their own. Nothing picked means
/// every cell.
fn picked(
    set: &ResultSet,
    edits: &BTreeMap<(usize, usize), Option<String>>,
    selection: &Selection,
) -> ResultSet {
    let (rows, columns): (Vec<usize>, Vec<usize>) = if selection.is_empty() {
        (
            (0..set.rows.len()).collect(),
            (0..set.columns.len()).collect(),
        )
    } else {
        (selection.rows(), selection.columns())
    };
    let mut out = ResultSet {
        columns: columns.iter().map(|c| set.columns[*c].clone()).collect(),
        ..ResultSet::default()
    };
    for row in rows {
        out.rows.push(columns.iter().map(|column| {
            let cell = (row, *column);
            if !selection.is_empty() && !selection.contains(cell) {
                return None;
            }
            match edits.get(&cell) {
                Some(change) => change.as_deref(),
                None => set.rows.get(row, *column).flatten(),
            }
        }));
    }
    out
}

impl CopyDialog {
    fn text(&mut self) -> &Result<String, String> {
        let stale = self
            .text
            .as_ref()
            .is_none_or(|(format, headers, _)| *format != self.format || *headers != self.headers);
        if stale {
            let dialect = self.dialect;
            let quote = move |name: &str| dialect.quote_ident(name);
            let options = export::Options {
                table: &self.table,
                quote: &quote,
                title: &self.title,
                headers: self.headers,
            };
            let text = export::render(&self.set, self.format, &options)
                .map_err(|error| error.to_string())
                .and_then(|bytes| String::from_utf8(bytes).map_err(|error| error.to_string()));
            self.text = Some((self.format, self.headers, text));
        }
        match &self.text {
            Some((_, _, text)) => text,
            None => unreachable!("the text was just made"),
        }
    }
}

/// Shows the dialog. Returns whether it is still open.
pub fn show(ctx: &egui::Context, dialog: &mut CopyDialog, actions: &mut Vec<Action>) -> bool {
    let palette = Palette::current(ctx);
    let mut open = true;
    let modal = egui::Modal::new(egui::Id::new("advanced-copy"))
        .frame(super::edit_flow::frame(&palette))
        .show(ctx, |ui| {
            ui.set_width(720.0);
            ui.spacing_mut().item_spacing.y = 12.0;
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Advanced copy")
                        .font(theme::semibold(16.0))
                        .color(palette.text),
                );
                let rows = dialog.set.rows.len();
                let columns = dialog.set.columns.len();
                ui.label(
                    RichText::new(format!(
                        "{} {} × {} {}",
                        thousands(rows),
                        if rows == 1 { "row" } else { "rows" },
                        thousands(columns),
                        if columns == 1 { "column" } else { "columns" },
                    ))
                    .color(palette.dim),
                );
            });

            // Left and right step through the formats.
            let formats: Vec<Format> = Format::ALL.into_iter().filter(|f| f.is_text()).collect();
            let step = ui.input_mut(|input| {
                if input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowRight) {
                    1
                } else if input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowLeft) {
                    -1
                } else {
                    0
                }
            });
            if step != 0
                && let Some(at) = formats.iter().position(|f| *f == dialog.format)
            {
                let next = (at as isize + step).rem_euclid(formats.len() as isize) as usize;
                dialog.format = formats[next];
            }
            let options: Vec<(Format, &str)> = formats.iter().map(|f| (*f, f.short())).collect();
            widgets::segmented(ui, &mut dialog.format, &options);

            let delimited = matches!(dialog.format, Format::Csv | Format::Tsv);
            ui.add_enabled_ui(delimited, |ui| {
                widgets::checkbox(ui, &mut dialog.headers, "Include column names");
            });

            let text = dialog.text().clone();
            egui::Frame::new()
                .fill(palette.window)
                .stroke(Stroke::new(1.0, palette.outline))
                .corner_radius(egui::CornerRadius::same(8))
                .inner_margin(egui::Margin::same(10))
                .show(ui, |ui| {
                    egui::ScrollArea::both()
                        .id_salt("advanced-copy-preview")
                        .max_height(320.0)
                        .auto_shrink([false, true])
                        .show(ui, |ui| match &text {
                            Ok(text) => {
                                let preview = match text.char_indices().nth(PREVIEW) {
                                    Some((end, _)) => format!("{}\n…", &text[..end]),
                                    None => text.clone(),
                                };
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(preview)
                                            .font(theme::mono(12.0))
                                            .color(palette.text),
                                    )
                                    .extend(),
                                );
                            }
                            Err(error) => {
                                ui.label(RichText::new(error).color(palette.danger));
                            }
                        });
                });

            let copy_key = ui.input_mut(|input| {
                input.consume_key(egui::Modifiers::NONE, egui::Key::Enter)
                    || input.consume_key(egui::Modifiers::COMMAND, egui::Key::Enter)
            });
            ui.horizontal(|ui| {
                ui.label(RichText::new("← → change the format · Enter copies").color(palette.dim));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let label = format!("Copy as {}", dialog.format.short());
                    let enabled = text.is_ok();
                    let clicked = ui
                        .add_enabled_ui(enabled, |ui| {
                            widgets::button(ui, Tone::Primary, Some(Icon::Copy), &label)
                        })
                        .inner
                        .clicked();
                    if enabled
                        && (clicked || copy_key)
                        && let Ok(text) = text
                    {
                        actions.push(Action::Copy(text));
                        actions.push(Action::RememberCopy(dialog.format, dialog.headers));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::selection::Mode;

    #[test]
    fn picks_the_selected_block() {
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
        set.rows.push([Some("a1"), Some("b1"), Some("c1")]);
        let mut selection = Selection::default();
        selection.click((0, 2), Mode::Replace);
        selection.click((1, 0), Mode::Toggle);
        let block = picked(&set, &BTreeMap::new(), &selection);
        let names: Vec<&str> = block.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["a", "c"]);
        assert_eq!(block.rows.get(0, 0), Some(None));
        assert_eq!(block.rows.get(0, 1), Some(Some("c0")));
        assert_eq!(block.rows.get(1, 0), Some(Some("a1")));
        // Nothing picked: everything.
        let all = picked(&set, &BTreeMap::new(), &Selection::default());
        assert_eq!((all.rows.len(), all.columns.len()), (2, 3));
    }
}
