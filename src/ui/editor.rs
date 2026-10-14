//! The SQL editor: a code text field with highlighting, line numbers, the
//! last error marked where the server pointed, and completion as you type.

use std::sync::Arc;

use egui::text::{CCursor, CCursorRange, LayoutJob};
use egui::{CornerRadius, Id, Key, Modifiers, Sense, Stroke, TextFormat, Ui, Vec2};
use violet_core::ConnectionId;

use super::action::Action;
use super::icons::Icon;
use super::theme::{self, Palette};
use super::widgets;
use super::workspace::QueryTab;
use crate::completion::{self, Catalog, Completion, Item, Kind};
use crate::sql::{self, TokenKind};

const FONT_SIZE: f32 = 13.5;
const GUTTER: f32 = 46.0;

/// Remembers the last layout so unchanged text is not re-lexed each frame.
#[derive(Default)]
pub struct Highlighter {
    /// The job holds the text it was made from: comparing against it is a
    /// `memcmp`, far cheaper than hashing the whole buffer every frame.
    job: Option<(LayoutJob, bool, Option<usize>)>,
}

impl Highlighter {
    fn job(&mut self, text: &str, palette: &Palette, error_at: Option<usize>) -> LayoutJob {
        let fresh = self.job.as_ref().is_some_and(|(job, dark, at)| {
            *dark == palette.dark && *at == error_at && job.text == text
        });
        if !fresh {
            self.job = Some((highlight(text, palette, error_at), palette.dark, error_at));
        }
        self.job
            .as_ref()
            .map(|(job, ..)| job.clone())
            .unwrap_or_default()
    }
}

#[hotpath::measure]
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

/// The completion list under the cursor.
#[derive(Default)]
pub struct Popup {
    open: bool,
    selected: usize,
    completion: Option<Completion>,
    /// The cursor the list was made for; moving away closes it.
    cursor: usize,
    /// Keyboard moved the selection: scroll it into view.
    reveal: bool,
    /// Where the list was drawn, so clicking it does not count as leaving
    /// the editor.
    area: Option<egui::Rect>,
}

impl Popup {
    fn close(&mut self) {
        self.open = false;
        self.completion = None;
        self.selected = 0;
        self.area = None;
    }

    fn items(&self) -> &[Item] {
        self.completion
            .as_ref()
            .map_or(&[], |completion| completion.items.as_slice())
    }

    fn showing(&self) -> bool {
        self.open && !self.items().is_empty()
    }
}

/// What completion knows about the editor's connection.
pub struct Assist<'a> {
    pub connection: Option<ConnectionId>,
    pub catalog: &'a dyn Catalog,
    pub options: completion::Options<'a>,
}

const ROW_HEIGHT: f32 = 24.0;
const VISIBLE_ROWS: usize = 10;

/// Draws `tab`'s editor filling the available space.
#[hotpath::measure]
pub fn show(ui: &mut Ui, tab: &mut QueryTab, assist: &Assist<'_>, actions: &mut Vec<Action>) {
    let palette = Palette::current(ui.ctx());
    let error_at = tab.error_offset();
    let QueryTab {
        id,
        sql,
        selection,
        highlighter,
        popup,
        ..
    } = tab;
    let text_id = Id::new(("violet-editor", *id));
    let focused = ui.ctx().memory(|memory| memory.has_focus(text_id));

    // An open list takes the keys it uses before the editor sees them.
    let mut accept = None;
    if focused && popup.open {
        let count = popup.items().len();
        ui.input_mut(|input| {
            if count > 0 {
                if input.consume_key(Modifiers::NONE, Key::ArrowDown) {
                    popup.selected = (popup.selected + 1) % count;
                    popup.reveal = true;
                }
                if input.consume_key(Modifiers::NONE, Key::ArrowUp) {
                    popup.selected = (popup.selected + count - 1) % count;
                    popup.reveal = true;
                }
                if input.consume_key(Modifiers::NONE, Key::Enter)
                    || input.consume_key(Modifiers::NONE, Key::Tab)
                {
                    accept = Some(popup.selected);
                }
            }
            if input.consume_key(Modifiers::NONE, Key::Escape) {
                popup.close();
            }
        });
    }
    let summoned = focused
        && ui.input_mut(|input| {
            let pressed = input.consume_key(Modifiers::CTRL, Key::Space);
            if pressed {
                // Some platforms type the space too.
                input
                    .events
                    .retain(|event| !matches!(event, egui::Event::Text(text) if text == " "));
            }
            pressed
        });
    if let Some(index) = accept {
        insert(ui.ctx(), text_id, sql, popup, index);
    }
    let over_list = popup
        .area
        .zip(ui.ctx().pointer_hover_pos())
        .is_some_and(|(area, pointer)| area.contains(pointer));

    let mut layouter = |ui: &Ui, buffer: &dyn egui::TextBuffer, wrap: f32| -> Arc<egui::Galley> {
        let mut job = highlighter.job(buffer.as_str(), &palette, error_at);
        job.wrap.max_width = wrap;
        ui.painter().layout_job(job)
    };

    let mut anchor = None;
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
                    .id(text_id)
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
                            "-- Write SQL here. ⌘↵ runs the statement under the cursor, ⌃Space completes.",
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
                follow(
                    popup,
                    sql,
                    selection,
                    output.response.changed(),
                    summoned,
                    output.response.has_focus() || over_list || accept.is_some(),
                );
                if popup.open {
                    refresh(popup, sql, selection.start, assist, actions);
                }
                if popup.showing()
                    && let Some(completion) = &popup.completion
                {
                    let start = sql[..completion.range.start.min(sql.len())].chars().count();
                    let rect = output.galley.pos_from_cursor(CCursor::new(start));
                    anchor = Some(output.galley_pos + rect.left_bottom().to_vec2());
                }

                // Line numbers: one per logical line, not per wrapped row.
                let painter = ui.painter();
                let right = gutter.right() - 8.0;
                let cursor_row = output
                    .cursor_range
                    .map(|range| output.galley.layout_from_cursor(range.primary).row);
                // Only rows inside the viewport are painted; the rest are
                // just counted, so long scripts cost no text layout.
                let visible = ui.clip_rect().y_range();
                let mut line = 1;
                let mut starts_line = true;
                for (index, row) in output.galley.rows.iter().enumerate() {
                    let top = output.galley_pos.y + row.pos.y;
                    if top > visible.max {
                        break;
                    }
                    if starts_line && top + 2.0 * FONT_SIZE >= visible.min {
                        let current = cursor_row == Some(index);
                        painter.text(
                            egui::pos2(right, top),
                            egui::Align2::RIGHT_TOP,
                            line.to_string(),
                            theme::mono(12.0),
                            if current {
                                palette.secondary
                            } else {
                                palette.dim.gamma_multiply(0.7)
                            },
                        );
                    }
                    if starts_line {
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

    if let Some(anchor) = anchor
        && let Some(index) = list(ui, *id, popup, anchor, &palette)
    {
        insert(ui.ctx(), text_id, sql, popup, index);
        ui.ctx().memory_mut(|memory| memory.request_focus(text_id));
        ui.ctx().request_repaint();
    }
}

/// Opens, keeps or closes the list after the editor had its turn.
fn follow(
    popup: &mut Popup,
    sql: &str,
    selection: &std::ops::Range<usize>,
    changed: bool,
    summoned: bool,
    focused: bool,
) {
    let cursor = selection.start;
    if !focused || !selection.is_empty() {
        popup.close();
        return;
    }
    if changed {
        // Typing a name or a dot opens it, anything else closes it.
        let typed = sql[..cursor.min(sql.len())].chars().next_back();
        if typed.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.') {
            popup.open = true;
            popup.completion = None;
        } else {
            popup.close();
        }
    } else if popup.open && cursor != popup.cursor {
        popup.close();
    }
    if summoned {
        popup.open = true;
        popup.completion = None;
    }
}

/// Recomputes the list when the text moved on, or while catalog data it
/// needs is still loading.
fn refresh(
    popup: &mut Popup,
    sql: &str,
    cursor: usize,
    assist: &Assist<'_>,
    actions: &mut Vec<Action>,
) {
    let stale = popup
        .completion
        .as_ref()
        .is_none_or(|completion| popup.cursor != cursor || !completion.needs.is_empty());
    if !stale {
        return;
    }
    let fresh = popup.completion.is_none() || popup.cursor != cursor;
    popup.cursor = cursor;
    let Some(completion) = completion::complete(sql, cursor, assist.catalog, &assist.options)
    else {
        popup.close();
        return;
    };
    if let Some(connection) = assist.connection
        && !completion.needs.is_empty()
    {
        actions.push(Action::LoadForCompletion(
            connection,
            completion.needs.clone(),
        ));
    }
    if completion.items.is_empty() && completion.needs.is_empty() {
        popup.close();
        return;
    }
    if fresh {
        popup.selected = 0;
        popup.reveal = true;
    }
    popup.selected = popup.selected.min(completion.items.len().saturating_sub(1));
    popup.completion = Some(completion);
}

/// Replaces the word at the cursor with the `index`th item.
fn insert(ctx: &egui::Context, text_id: Id, sql: &mut String, popup: &mut Popup, index: usize) {
    let Some(completion) = popup.completion.take() else {
        return;
    };
    popup.close();
    let Some(item) = completion.items.get(index) else {
        return;
    };
    let range = completion.range.start.min(sql.len())..completion.range.end.min(sql.len());
    sql.replace_range(range.clone(), &item.insert);
    let caret = sql[..range.start + item.caret].chars().count();
    let mut state = egui::text_edit::TextEditState::load(ctx, text_id).unwrap_or_default();
    state
        .cursor
        .set_char_range(Some(CCursorRange::one(CCursor::new(caret))));
    state.store(ctx, text_id);
}

fn kind_style(kind: Kind, palette: &Palette) -> (Icon, egui::Color32) {
    let syntax = palette.syntax;
    match kind {
        Kind::Join => (Icon::Key, palette.success),
        Kind::Column => (Icon::Columns, palette.secondary),
        Kind::Alias => (Icon::Rows, syntax.quoted),
        Kind::Table => (Icon::Table, palette.accent),
        Kind::View => (Icon::View, palette.accent),
        Kind::Schema => (Icon::Schema, palette.warning),
        Kind::Function => (Icon::Function, syntax.parameter),
        Kind::Keyword => (Icon::Zap, syntax.keyword),
        Kind::Type => (Icon::Sequence, syntax.kind),
    }
}

/// Draws the list at `anchor`, below the word. Returns a clicked item.
fn list(
    ui: &Ui,
    tab: u64,
    popup: &mut Popup,
    anchor: egui::Pos2,
    palette: &Palette,
) -> Option<usize> {
    let completion = popup.completion.as_ref()?;
    let prefix = completion.prefix.to_lowercase();
    let mut clicked = None;
    let reveal = std::mem::take(&mut popup.reveal);
    let selected = popup.selected;
    let loading = !completion.needs.is_empty();
    let area = egui::Area::new(Id::new(("completion", tab)))
        .order(egui::Order::Foreground)
        .fixed_pos(anchor + Vec2::new(-6.0, 4.0))
        .show(ui.ctx(), |ui| {
            egui::Frame::new()
                .fill(palette.surface)
                .stroke(Stroke::new(1.0, palette.outline))
                .corner_radius(CornerRadius::same(8))
                .shadow(ui.ctx().global_style().visuals.popup_shadow)
                .inner_margin(egui::Margin::same(4))
                .show(ui, |ui| {
                    let width = 420.0;
                    ui.set_width(width);
                    egui::ScrollArea::vertical()
                        .id_salt(("completion-scroll", tab))
                        .max_height(ROW_HEIGHT * VISIBLE_ROWS as f32)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            ui.spacing_mut().item_spacing.y = 0.0;
                            for (index, item) in completion.items.iter().enumerate() {
                                let (rect, response) = ui.allocate_exact_size(
                                    Vec2::new(ui.available_width(), ROW_HEIGHT),
                                    Sense::click(),
                                );
                                if index == selected && reveal {
                                    ui.scroll_to_rect(rect, None);
                                }
                                if !ui.is_rect_visible(rect) {
                                    continue;
                                }
                                row(
                                    ui,
                                    rect,
                                    item,
                                    &prefix,
                                    index == selected,
                                    response.hovered(),
                                    palette,
                                );
                                if response.clicked() {
                                    clicked = Some(index);
                                }
                            }
                        });
                    ui.add_space(3.0);
                    ui.horizontal(|ui| {
                        ui.add_space(6.0);
                        if loading {
                            ui.add(egui::Spinner::new().size(10.0).color(palette.dim));
                            ui.label(
                                egui::RichText::new("Reading the catalog…")
                                    .size(10.5)
                                    .color(palette.dim),
                            );
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.add_space(6.0);
                            ui.label(
                                egui::RichText::new("↵ insert   ↑↓ choose   esc close")
                                    .size(10.5)
                                    .color(palette.dim),
                            );
                        });
                    });
                });
        });
    popup.area = Some(area.response.rect);
    clicked
}

fn row(
    ui: &Ui,
    rect: egui::Rect,
    item: &Item,
    prefix: &str,
    selected: bool,
    hovered: bool,
    palette: &Palette,
) {
    let painter = ui.painter();
    if selected {
        painter.rect_filled(rect, CornerRadius::same(5), palette.accent_soft);
    } else if hovered {
        painter.rect_filled(rect, CornerRadius::same(5), palette.surface_hover);
    }
    let (icon, tint) = kind_style(item.kind, palette);
    let icon_rect = egui::Rect::from_center_size(
        egui::pos2(rect.left() + 14.0, rect.center().y),
        Vec2::splat(13.0),
    );
    widgets::paint_icon(ui, icon, icon_rect, tint);

    // The typed part of the name in the accent colour.
    let font = theme::mono(12.5);
    let text = if selected {
        palette.text
    } else {
        palette.secondary
    };
    let mut job = LayoutJob::default();
    let matched = if !prefix.is_empty() && item.label.to_lowercase().starts_with(prefix) {
        prefix.len()
    } else {
        0
    };
    let split = (0..=matched)
        .rev()
        .find(|at| item.label.is_char_boundary(*at))
        .unwrap_or(0);
    let format = |color| TextFormat::simple(font.clone(), color);
    job.append(&item.label[..split], 0.0, format(palette.accent));
    job.append(&item.label[split..], 0.0, format(text));
    job.wrap = egui::text::TextWrapping {
        max_width: if item.detail.is_empty() {
            rect.width() - 36.0
        } else {
            rect.width() * 0.68
        }
        .max(1.0),
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some('…'),
    };
    let galley = painter.layout_job(job);
    let label_pos = egui::pos2(rect.left() + 28.0, rect.center().y - galley.size().y / 2.0);
    let label_width = galley.size().x;
    painter.galley(label_pos, galley, text);

    if !item.detail.is_empty() {
        let left = label_pos.x + label_width + 12.0;
        let detail_rect = egui::Rect::from_min_max(
            egui::pos2(left, rect.top()),
            rect.right_bottom() - Vec2::new(8.0, 0.0),
        );
        if detail_rect.width() > 24.0 {
            widgets::paint_text(
                ui,
                detail_rect,
                egui::Align2::RIGHT_CENTER,
                &item.detail,
                theme::medium(11.0),
                palette.dim,
            );
        }
    }
}
