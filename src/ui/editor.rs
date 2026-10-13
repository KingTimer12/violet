//! The SQL editor: a code text field with highlighting, line numbers and
//! the last error marked where the server pointed.

use std::hash::{Hash as _, Hasher as _};
use std::sync::Arc;

use egui::text::{CCursor, LayoutJob};
use egui::{Stroke, TextFormat, Ui};

use super::theme::{self, Palette};
use super::workspace::QueryTab;
use crate::sql::{self, TokenKind};

const FONT_SIZE: f32 = 13.5;
const GUTTER: f32 = 46.0;

/// Remembers the last layout so unchanged text is not re-lexed each frame.
#[derive(Default)]
pub struct Highlighter {
    key: u64,
    job: Option<LayoutJob>,
}

impl Highlighter {
    fn job(&mut self, text: &str, palette: &Palette, error_at: Option<usize>) -> LayoutJob {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut hasher);
        palette.dark.hash(&mut hasher);
        error_at.hash(&mut hasher);
        let key = hasher.finish();
        if self.key != key || self.job.is_none() {
            self.key = key;
            self.job = Some(highlight(text, palette, error_at));
        }
        self.job.clone().unwrap_or_default()
    }
}

fn highlight(text: &str, palette: &Palette, error_at: Option<usize>) -> LayoutJob {
    let syntax = palette.syntax;
    let font = theme::mono(FONT_SIZE);
    let mut job = LayoutJob::default();
    for token in sql::tokens(text) {
        let color = match token.kind {
            TokenKind::Keyword => syntax.keyword,
            TokenKind::Type => syntax.kind,
            TokenKind::String => syntax.string,
            TokenKind::Number => syntax.number,
            TokenKind::Comment => syntax.comment,
            TokenKind::QuotedIdentifier => syntax.quoted,
            TokenKind::Parameter => syntax.parameter,
            TokenKind::Operator | TokenKind::Punctuation => syntax.operator,
            TokenKind::Identifier | TokenKind::Whitespace => palette.text,
        };
        let mut format = TextFormat {
            font_id: font.clone(),
            color,
            italics: token.kind == TokenKind::Comment,
            ..TextFormat::default()
        };
        let is_error = error_at.is_some_and(|at| {
            token.kind != TokenKind::Whitespace
                && (token.range.contains(&at) || (at == text.len() && token.range.end == at))
        });
        if is_error {
            format.underline = Stroke::new(1.5, palette.danger);
            format.background = palette.danger.gamma_multiply(0.18);
        }
        job.append(&text[token.range], 0.0, format);
    }
    job
}

fn byte_offset(text: &str, cursor: CCursor) -> usize {
    text.char_indices()
        .nth(cursor.index.0)
        .map_or(text.len(), |(at, _)| at)
}

/// Draws `tab`'s editor filling the available space.
pub fn show(ui: &mut Ui, tab: &mut QueryTab) {
    let palette = Palette::current(ui.ctx());
    let error_at = tab.error_offset();
    let QueryTab {
        id,
        sql,
        selection,
        highlighter,
        ..
    } = tab;

    let mut layouter = |ui: &Ui, buffer: &dyn egui::TextBuffer, wrap: f32| -> Arc<egui::Galley> {
        let mut job = highlighter.job(buffer.as_str(), &palette, error_at);
        job.wrap.max_width = wrap;
        ui.painter().layout_job(job)
    };

    egui::ScrollArea::vertical()
        .id_salt(("editor", *id))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let height = ui.available_height();
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                // The gutter is its own column so it never covers text.
                let (gutter, _) =
                    ui.allocate_exact_size(egui::vec2(GUTTER, height), egui::Sense::hover());
                let output = egui::TextEdit::multiline(sql)
                    .id_salt(("editor-text", *id))
                    .code_editor()
                    .frame(egui::Frame::NONE)
                    .lock_focus(true)
                    .desired_width(f32::INFINITY)
                    .min_size(egui::vec2(0.0, height))
                    .margin(egui::Margin {
                        left: 10,
                        right: 12,
                        top: 12,
                        bottom: 12,
                    })
                    .hint_text(
                        egui::RichText::new(
                            "-- Write SQL here. ⌘↵ runs the statement under the cursor.",
                        )
                        .font(theme::mono(FONT_SIZE))
                        .color(palette.dim),
                    )
                    .layouter(&mut layouter)
                    .show(ui);

                if let Some(range) = output.cursor_range {
                    let [start, end] = range.sorted_cursors();
                    *selection = byte_offset(sql, start)..byte_offset(sql, end);
                }

                // Line numbers: one per logical line, not per wrapped row.
                let painter = ui.painter();
                let right = gutter.right() - 8.0;
                let cursor_row = output
                    .cursor_range
                    .map(|range| output.galley.layout_from_cursor(range.primary).row);
                let mut line = 1;
                let mut starts_line = true;
                for (index, row) in output.galley.rows.iter().enumerate() {
                    if starts_line {
                        let current = cursor_row == Some(index);
                        painter.text(
                            egui::pos2(right, output.galley_pos.y + row.pos.y),
                            egui::Align2::RIGHT_TOP,
                            line.to_string(),
                            theme::mono(12.0),
                            if current {
                                palette.secondary
                            } else {
                                palette.dim.gamma_multiply(0.7)
                            },
                        );
                        line += 1;
                    }
                    starts_line = row.ends_with_newline;
                }
                if output.galley.rows.is_empty() {
                    painter.text(
                        egui::pos2(right, output.galley_pos.y),
                        egui::Align2::RIGHT_TOP,
                        "1",
                        theme::mono(12.0),
                        palette.dim,
                    );
                }
            });
        });
}
