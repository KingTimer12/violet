//! The SQL editor: a code text field with highlighting, line numbers, the
//! last error marked where the server pointed, completion as you type, a
//! find and replace bar, and lines commented out with a shortcut.

use std::ops::Range;
use std::sync::Arc;

use egui::text::{CCursor, CCursorRange, LayoutJob};
use egui::{CornerRadius, Id, Key, Modifiers, Sense, Stroke, TextFormat, Ui, Vec2};
use violet_core::{ConnectionId, Language};

use super::action::Action;
use super::icons::Icon;
use super::shortcuts::{self, Command};
use super::theme::{self, Palette};
use super::widgets::{self, Tone};
use super::workspace::QueryTab;
use crate::completion::{self, Catalog, Completion, Item, Kind};
use crate::diagnostics::{self, Diagnostic, Severity};
use crate::editing::{self, Edited, Snippet};
use crate::sql::{self, TokenKind};

const FONT_SIZE: f32 = 13.5;
const GUTTER: f32 = 46.0;

/// Remembers the last layout so unchanged text is not re-lexed each frame.
#[derive(Default)]
pub struct Highlighter {
    /// The job holds the text it was made from: comparing against it is a
    /// `memcmp`, far cheaper than hashing the whole buffer every frame.
    job: Option<(LayoutJob, bool, Option<usize>, Language, Marks)>,
}

/// The ranges the editor underlines for the problems it found.
type Marks = Vec<(Range<usize>, Severity)>;

impl Highlighter {
    fn job(
        &mut self,
        text: &str,
        palette: &Palette,
        error_at: Option<usize>,
        language: Language,
        marks: &Marks,
    ) -> LayoutJob {
        let fresh = self
            .job
            .as_ref()
            .is_some_and(|(job, dark, at, lang, kept)| {
                *dark == palette.dark
                    && *at == error_at
                    && *lang == language
                    && kept == marks
                    && job.text == text
            });
        if !fresh {
            self.job = Some((
                highlight(text, palette, error_at, language, marks),
                palette.dark,
                error_at,
                language,
                marks.clone(),
            ));
        }
        self.job
            .as_ref()
            .map(|(job, ..)| job.clone())
            .unwrap_or_default()
    }
}

/// `text` coloured as the editor colours it, for previews.
pub fn highlighted(text: &str, palette: &Palette, language: Language) -> LayoutJob {
    highlight(text, palette, None, language, &Vec::new())
}

/// Moves `tab`'s editor cursor to `char_index`, and focuses it.
pub fn place_cursor(ctx: &egui::Context, tab: u64, char_index: usize) {
    let text_id = Id::new(("violet-editor", tab));
    let mut state = egui::text_edit::TextEditState::load(ctx, text_id).unwrap_or_default();
    state
        .cursor
        .set_char_range(Some(CCursorRange::one(CCursor::new(char_index))));
    state.store(ctx, text_id);
    ctx.memory_mut(|memory| memory.request_focus(text_id));
}

/// Redis tokens as the SQL colours name them.
fn redis_tokens(text: &str) -> impl Iterator<Item = sql::Token> {
    use violet_lang::redis::TokenKind as Redis;
    violet_lang::redis::tokens(text)
        .into_iter()
        .map(|token| sql::Token {
            kind: match token.kind {
                Redis::Command => TokenKind::Keyword,
                Redis::Unknown | Redis::Argument => TokenKind::Identifier,
                Redis::String => TokenKind::String,
                Redis::Number => TokenKind::Number,
                Redis::Comment => TokenKind::Comment,
                Redis::Whitespace => TokenKind::Whitespace,
            },
            range: token.range,
        })
}

/// Replaces `tab`'s editor text and selects the byte range `selection`
/// of it.
pub fn replace_text(ctx: &egui::Context, tab: &mut QueryTab, edited: Edited) {
    let text_id = Id::new(("violet-editor", tab.id));
    tab.sql = edited.text;
    select(ctx, text_id, &tab.sql, edited.selection.clone());
    tab.selection = edited.selection;
}

/// Selects the byte range `range` of the editor's `text`.
fn select(ctx: &egui::Context, text_id: Id, text: &str, range: Range<usize>) {
    let chars = |at: usize| text[..at.min(text.len())].chars().count();
    let mut state = egui::text_edit::TextEditState::load(ctx, text_id).unwrap_or_default();
    state.cursor.set_char_range(Some(CCursorRange::two(
        CCursor::new(chars(range.start)),
        CCursor::new(chars(range.end)),
    )));
    state.store(ctx, text_id);
}

/// Finding, and replacing, text in an editor.
#[derive(Default)]
pub struct FindBar {
    query: String,
    replacement: String,
    replacing: bool,
    /// Letters match only in the case typed.
    case: bool,
    /// The match picked, by its place among them; `None` picks the first
    /// after the cursor.
    current: Option<usize>,
    /// The find field takes the keyboard, its text selected.
    focus: bool,
    /// The picked match is selected and scrolled into view.
    reveal: bool,
}

/// Opens `tab`'s find bar, seeded with the selected text when it is one
/// line; `replacing` shows the replace field too.
pub fn open_find(tab: &mut QueryTab, replacing: bool) {
    open_bar(&mut tab.find, &tab.sql, &tab.selection, replacing);
}

fn open_bar(find: &mut Option<FindBar>, sql: &str, selection: &Range<usize>, replacing: bool) {
    let picked = sql
        .get(selection.clone())
        .filter(|text| !text.is_empty() && !text.contains('\n'))
        .map(str::to_owned);
    let bar = find.get_or_insert_with(FindBar::default);
    if let Some(picked) = picked {
        bar.query = picked;
    }
    bar.replacing |= replacing;
    bar.current = None;
    bar.focus = true;
    bar.reveal = true;
}

/// What the find bar asks of the editor.
enum FindStep {
    Stay,
    /// The query or its case changed: pick the first match after the
    /// cursor again.
    Changed,
    Next,
    Previous,
    Replace,
    ReplaceAll,
    Close,
}

fn find_id(tab: u64) -> Id {
    Id::new(("violet-find", tab))
}

fn replace_id(tab: u64) -> Id {
    Id::new(("violet-replace", tab))
}

/// The bar above the editor. `count` is how many matches there are.
fn find_bar(ui: &mut Ui, tab: u64, bar: &mut FindBar, count: usize) -> FindStep {
    let palette = Palette::current(ui.ctx());
    let mut step = FindStep::Stay;
    let pressed = |ui: &Ui, key: Key| ui.input(|input| input.key_pressed(key));
    egui::Frame::new()
        .fill(palette.panel)
        .stroke(Stroke::new(1.0, palette.outline))
        .inner_margin(egui::Margin::symmetric(8, 6))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
            ui.horizontal(|ui| {
                if std::mem::take(&mut bar.focus) {
                    // Typing replaces what the field held.
                    let mut state = egui::text_edit::TextEditState::load(ui.ctx(), find_id(tab))
                        .unwrap_or_default();
                    state.cursor.set_char_range(Some(CCursorRange::two(
                        CCursor::new(0),
                        CCursor::new(bar.query.chars().count()),
                    )));
                    state.store(ui.ctx(), find_id(tab));
                    ui.ctx()
                        .memory_mut(|memory| memory.request_focus(find_id(tab)));
                }
                let field = ui.add_sized(
                    [260.0, 28.0],
                    widgets::text_input(&mut bar.query, "Find").id(find_id(tab)),
                );
                if field.changed() {
                    step = FindStep::Changed;
                }
                if field.lost_focus() {
                    if pressed(ui, Key::Escape) {
                        step = FindStep::Close;
                    } else if pressed(ui, Key::Enter) {
                        step = if ui.input(|input| input.modifiers.shift) {
                            FindStep::Previous
                        } else {
                            FindStep::Next
                        };
                        field.request_focus();
                    }
                }
                let tone = if bar.case { Tone::Primary } else { Tone::Ghost };
                if widgets::button(ui, tone, None, "Aa")
                    .on_hover_text("Match case")
                    .clicked()
                {
                    bar.case = !bar.case;
                    step = FindStep::Changed;
                }
                let found = match (count, bar.current) {
                    _ if bar.query.is_empty() => String::new(),
                    (0, _) => "No results".to_owned(),
                    (count, Some(current)) => format!("{} of {count}", current + 1),
                    (count, None) => format!("{count} found"),
                };
                ui.label(egui::RichText::new(found).color(palette.dim));
                if widgets::icon_button(ui, Icon::ArrowUp, "Previous (Shift+Enter)").clicked() {
                    step = FindStep::Previous;
                }
                if widgets::icon_button(ui, Icon::ArrowDown, "Next (Enter)").clicked() {
                    step = FindStep::Next;
                }
                let tone = if bar.replacing {
                    Tone::Primary
                } else {
                    Tone::Ghost
                };
                let replace_tip = format!(
                    "Replace ({})",
                    ui.ctx().format_shortcut(&shortcuts::get(Command::Replace))
                );
                if widgets::button(ui, tone, None, "Replace")
                    .on_hover_text(replace_tip)
                    .clicked()
                {
                    bar.replacing = !bar.replacing;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if widgets::icon_button(ui, Icon::Close, "Close (Esc)").clicked() {
                        step = FindStep::Close;
                    }
                });
            });
            if bar.replacing {
                ui.horizontal(|ui| {
                    let field = ui.add_sized(
                        [260.0, 28.0],
                        widgets::text_input(&mut bar.replacement, "Replace with")
                            .id(replace_id(tab)),
                    );
                    if field.lost_focus() {
                        if pressed(ui, Key::Escape) {
                            step = FindStep::Close;
                        } else if pressed(ui, Key::Enter) {
                            step = FindStep::Replace;
                            field.request_focus();
                        }
                    }
                    ui.add_enabled_ui(count > 0, |ui| {
                        if widgets::button(ui, Tone::Secondary, None, "Replace")
                            .on_hover_text("Replace this match, and go to the next (Enter)")
                            .clicked()
                        {
                            step = FindStep::Replace;
                        }
                        if widgets::button(ui, Tone::Secondary, None, "Replace all").clicked() {
                            step = FindStep::ReplaceAll;
                        }
                    });
                });
            }
        });
    step
}

#[hotpath::measure]
fn highlight(
    text: &str,
    palette: &Palette,
    error_at: Option<usize>,
    language: Language,
    marks: &Marks,
) -> LayoutJob {
    let syntax = palette.syntax;
    let font = theme::mono(FONT_SIZE);
    let mut job = LayoutJob::default();
    let tokens: Box<dyn Iterator<Item = sql::Token>> = match language {
        Language::Redis => Box::new(redis_tokens(text)),
        Language::Sql | Language::Mongodb => Box::new(sql::tokens(text)),
    };
    for token in tokens {
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
        } else if let Some((_, severity)) = marks
            .iter()
            .find(|(range, _)| range.start < token.range.end && token.range.start < range.end)
        {
            let color = match severity {
                Severity::Error => palette.danger,
                Severity::Warning => palette.warning,
            };
            format.underline = Stroke::new(1.5, color);
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
    /// The connection the list was made for: switching it remakes the
    /// list in the new connection's language.
    connection: Option<ConnectionId>,
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

/// What Tab does while a list is open or a snippet has stops left.
#[derive(Debug, PartialEq, Eq)]
enum Tab {
    /// Accepts the item chosen in the list.
    Item,
    /// Moves to the next stop of the snippet.
    Stop,
    /// Is left to the editor.
    Plain,
}

/// Who takes Tab: a snippet's stops come before the list, which Enter
/// still accepts.
fn tab_picks(list_open: bool, stops_left: bool) -> Tab {
    if stops_left {
        Tab::Stop
    } else if list_open {
        Tab::Item
    } else {
        Tab::Plain
    }
}

/// What completion knows about the editor's connection.
pub struct Assist<'a> {
    pub connection: Option<ConnectionId>,
    pub language: Language,
    pub catalog: &'a dyn Catalog,
    pub options: completion::Options<'a>,
    /// For a key-value store: the database the editor is on, and its keys
    /// known so far.
    pub keys: Option<completion::Keyspace<'a>>,
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
        snippet,
        find,
        diagnostics,
        ..
    } = tab;
    let text_id = Id::new(("violet-editor", *id));
    let focused = ui.ctx().memory(|memory| memory.has_focus(text_id));
    let bar_focused = ui
        .ctx()
        .memory(|memory| memory.has_focus(find_id(*id)) || memory.has_focus(replace_id(*id)));

    // Find, replace and format, from the editor, its find bar, or when no
    // field has the keyboard.
    if focused || bar_focused || !ui.ctx().egui_wants_keyboard_input() {
        let (format, find_key, replace_key) = ui.input_mut(|input| {
            (
                shortcuts::consume(input, Command::Format),
                shortcuts::consume(input, Command::Find),
                shortcuts::consume(input, Command::Replace),
            )
        });
        if format {
            actions.push(Action::FormatSql(*id));
        }
        if find_key || replace_key {
            open_bar(find, sql, selection, replace_key);
        }
    }
    if focused && ui.input_mut(|input| shortcuts::consume(input, Command::Comment)) {
        let edited = editing::toggle_comment(sql, selection.clone());
        *sql = edited.text;
        select(ui.ctx(), text_id, sql, edited.selection.clone());
        *selection = edited.selection;
    }
    if focused
        && find.is_some()
        && !popup.open
        && ui.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Escape))
    {
        *find = None;
    }

    // The find bar, and what it asks.
    let mut matches: Vec<Range<usize>> = Vec::new();
    let mut picked = None;
    let mut reveal = false;
    if let Some(bar) = find.as_mut() {
        matches = editing::find_all(sql, &bar.query, bar.case);
        let step = find_bar(ui, *id, bar, matches.len());
        let after_cursor = |matches: &[Range<usize>]| {
            matches
                .iter()
                .position(|found| found.start >= selection.start)
                .or((!matches.is_empty()).then_some(0))
        };
        match step {
            FindStep::Stay => {}
            FindStep::Changed => {
                bar.current = None;
                bar.reveal = true;
            }
            FindStep::Next | FindStep::Previous if !matches.is_empty() => {
                let count = matches.len();
                bar.current = Some(match (bar.current, matches!(step, FindStep::Next)) {
                    (Some(current), true) => (current + 1) % count,
                    (Some(current), false) => (current + count - 1) % count,
                    (None, _) => after_cursor(&matches).unwrap_or(0),
                });
                bar.reveal = true;
            }
            FindStep::Next | FindStep::Previous => {}
            FindStep::Replace => {
                if let Some(found) = bar.current.and_then(|at| matches.get(at)).cloned() {
                    sql.replace_range(found, &bar.replacement);
                    matches = editing::find_all(sql, &bar.query, bar.case);
                    // The match after it is now at its place.
                    bar.current = bar
                        .current
                        .filter(|at| *at < matches.len())
                        .or(after_cursor(&matches));
                    bar.reveal = true;
                } else {
                    bar.current = after_cursor(&matches);
                    bar.reveal = true;
                }
            }
            FindStep::ReplaceAll => {
                *sql = editing::replace_all(sql, &matches, &bar.replacement);
                matches = editing::find_all(sql, &bar.query, bar.case);
                bar.current = None;
            }
            FindStep::Close => {
                ui.ctx().memory_mut(|memory| memory.request_focus(text_id));
            }
        }
        if matches!(step, FindStep::Close) {
            *find = None;
            matches.clear();
        } else {
            if bar.current.is_none() && bar.reveal {
                bar.current = after_cursor(&matches);
            }
            bar.current = bar.current.filter(|at| *at < matches.len());
            picked = bar.current;
            reveal = std::mem::take(&mut bar.reveal) && picked.is_some();
            if reveal && let Some(found) = picked.and_then(|at| matches.get(at)) {
                select(ui.ctx(), text_id, sql, found.clone());
                *selection = found.clone();
            }
        }
    }

    // An open list takes the keys it uses before the editor sees them.
    let pending = snippet.as_ref().is_some_and(|open| !open.is_empty());
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
                    || (tab_picks(true, pending) == Tab::Item
                        && input.consume_key(Modifiers::NONE, Key::Tab))
                {
                    accept = Some(popup.selected);
                }
            }
            if input.consume_key(Modifiers::NONE, Key::Escape) {
                popup.close();
            }
        });
    }
    // Tab moves through what is left of an inserted snippet, even over a
    // list the typing in a placeholder opened.
    if focused
        && tab_picks(popup.open, pending) == Tab::Stop
        && let Some(open) = snippet.as_mut()
        && ui.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Tab))
    {
        popup.close();
        if let Some(stop) = open.next(selection.start) {
            select(ui.ctx(), text_id, sql, stop.clone());
            *selection = stop;
        }
        if open.is_empty() {
            *snippet = None;
        }
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
        let item = insert(ui.ctx(), text_id, sql, popup, snippet, index);
        accepted(assist, item, actions);
    }
    let over_list = popup
        .area
        .zip(ui.ctx().pointer_hover_pos())
        .is_some_and(|(area, pointer)| area.contains(pointer));

    // Problems are read once the text stands still, and again as the
    // catalog fills in.
    if assist.language == Language::Sql {
        let now = std::time::Instant::now();
        if diagnostics.due(sql, now) {
            let found = diagnostics::diagnose(sql, assist.catalog, &assist.options);
            diagnostics.store(found, now);
        }
        if let Some(wait) = diagnostics.wait(now) {
            ui.ctx().request_repaint_after(wait);
        }
    }
    let marks = diagnostics.marks(selection.start);
    let mut layouter = |ui: &Ui, buffer: &dyn egui::TextBuffer, wrap: f32| -> Arc<egui::Galley> {
        let mut job = highlighter.job(buffer.as_str(), &palette, error_at, assist.language, &marks);
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
                // The text edit only takes clicks on the rows it is sized
                // for (`min_size` leaves its height alone), so it asks for
                // as many as fill the pane: a click below the text still
                // lands in it.
                let font = theme::mono(FONT_SIZE);
                let line_height = ui.fonts_mut(|fonts| fonts.row_height(&font))
                    + ui.spacing().extra_text_line_spacing;
                let rows = (height / line_height).floor().max(1.0) as usize;
                let output = egui::TextEdit::multiline(sql)
                    .id(text_id)
                    .code_editor()
                    .font(font)
                    .desired_rows(rows)
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
                        egui::RichText::new({
                            let run = ui
                                .ctx()
                                .format_shortcut(&shortcuts::get(Command::RunStatement));
                            match assist.language {
                                Language::Redis => format!(
                                    "# Write Redis commands here, one per line. {run} runs the one under the cursor (a MULTI … EXEC block whole), ⌃Space completes."
                                ),
                                _ => format!(
                                    "-- Write SQL here. {run} runs the statement under the cursor, ⌃Space completes."
                                ),
                            }
                        })
                        .font(theme::mono(FONT_SIZE))
                        .color(palette.dim),
                    )
                    .layouter(&mut layouter)
                    .show(ui);

                autoscroll(ui, &output.response);

                if let Some(range) = output.cursor_range {
                    let [start, end] = range.sorted_cursors();
                    *selection = byte_offset(sql, start)..byte_offset(sql, end);
                }
                if output.response.changed()
                    && let Some(open) = snippet.as_mut()
                    && !open.follow(sql.len(), selection.start)
                {
                    *snippet = None;
                }
                follow(
                    popup,
                    sql,
                    selection,
                    output.response.changed(),
                    summoned,
                    output.response.has_focus() || over_list || accept.is_some(),
                    assist.language,
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

                // Tooltips for the word under the pointer; the syntax of the
                // command being written, under the cursor.
                if !popup.showing() {
                    let hint = hovered_word(&output, sql, assist, diagnostics.found());
                    if let Some(hint) = hint {
                        output
                            .response
                            .response
                            .clone()
                            .on_hover_ui_at_pointer(|ui| hover_card(ui, &hint, &palette));
                    }
                    let signature = if !output.response.has_focus() {
                        None
                    } else {
                        match assist.language {
                            Language::Redis => completion::redis_signature(sql, selection.start),
                            Language::Sql => completion::sql_signature(
                                sql,
                                selection.start,
                                assist.options.family,
                            ),
                            Language::Mongodb => None,
                        }
                    };
                    if let Some(signature) = signature {
                        let at = sql[..selection.start].chars().count();
                        let rect = output.galley.pos_from_cursor(CCursor::new(at));
                        let anchor = output.galley_pos + rect.left_bottom().to_vec2();
                        signature_card(ui, *id, anchor, &signature, &palette);
                    }
                }

                // Matches of the find bar, the picked one stronger.
                let mut chars = 0;
                let mut at = 0;
                for (index, found) in matches.iter().enumerate() {
                    chars += sql[at..found.start].chars().count();
                    let start = chars;
                    chars += sql[found.clone()].chars().count();
                    at = found.end;
                    let from = output.galley.pos_from_cursor(CCursor::new(start));
                    let to = output.galley.pos_from_cursor(CCursor::new(chars));
                    let right = if to.top() == from.top() {
                        to.left()
                    } else {
                        from.left() + 8.0
                    };
                    let rect = egui::Rect::from_min_max(from.min, egui::pos2(right, from.max.y))
                        .translate(output.galley_pos.to_vec2());
                    let current = picked == Some(index);
                    let fill = if current {
                        palette.accent.gamma_multiply(0.45)
                    } else {
                        palette.warning.gamma_multiply(0.28)
                    };
                    ui.painter().rect_filled(rect, CornerRadius::same(2), fill);
                    if current && reveal {
                        ui.scroll_to_rect(rect.expand(24.0), Some(egui::Align::Center));
                    }
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
        let item = insert(ui.ctx(), text_id, sql, popup, snippet, index);
        accepted(assist, item, actions);
        ui.ctx().memory_mut(|memory| memory.request_focus(text_id));
        ui.ctx().request_repaint();
    }
}

/// What the word under the pointer is, when the pointer is on one.
fn hovered_word(
    output: &egui::text_edit::TextEditOutput,
    sql: &str,
    assist: &Assist<'_>,
    problems: &[Diagnostic],
) -> Option<completion::Hover> {
    let pointer = output.response.hover_pos()?;
    let local = pointer - output.galley_pos;
    let index = output.galley.cursor_from_pos(local).index.0;
    let offset = sql
        .char_indices()
        .nth(index)
        .map_or(sql.len(), |(at, _)| at);
    let problem = problems
        .iter()
        .find(|problem| problem.range.contains(&offset))
        .map(|problem| completion::Hover {
            range: problem.range.clone(),
            title: problem.message.clone(),
            detail: String::new(),
        });
    let hint = problem.or_else(|| match assist.language {
        Language::Redis => completion::redis_hover(sql, offset),
        Language::Sql => completion::hover(sql, offset, assist.catalog, &assist.options),
        Language::Mongodb => completion::mongo_hover(sql, offset),
    })?;
    // The nearest character is not the one under the pointer past a row's
    // end or off its word.
    let chars = |at: usize| sql[..at].chars().count();
    let from = output
        .galley
        .pos_from_cursor(CCursor::new(chars(hint.range.start)));
    let to = output
        .galley
        .pos_from_cursor(CCursor::new(chars(hint.range.end)));
    let on_word = local.y >= from.top()
        && local.y <= from.bottom()
        && to.top() == from.top()
        && local.x >= from.left()
        && local.x <= to.right();
    on_word.then_some(hint)
}

/// The tooltip of a word: what it is, and what is known of it.
fn hover_card(ui: &mut Ui, hint: &completion::Hover, palette: &Palette) {
    ui.set_max_width(420.0);
    ui.label(
        egui::RichText::new(&hint.title)
            .font(theme::mono(FONT_SIZE))
            .color(palette.text),
    );
    if !hint.detail.is_empty() {
        ui.label(egui::RichText::new(&hint.detail).color(palette.dim));
    }
}

/// A command's or function's syntax under the cursor, the argument being written marked.
fn signature_card(
    ui: &Ui,
    tab: u64,
    anchor: egui::Pos2,
    signature: &completion::Signature,
    palette: &Palette,
) {
    let font = theme::mono(12.0);
    let mut job = LayoutJob::default();
    let plain = TextFormat {
        font_id: font.clone(),
        color: palette.dim,
        ..TextFormat::default()
    };
    let active = TextFormat {
        font_id: font,
        color: palette.accent,
        underline: Stroke::new(1.0, palette.accent),
        ..TextFormat::default()
    };
    let label = &signature.label;
    match &signature.active {
        Some(range) => {
            job.append(&label[..range.start], 0.0, plain.clone());
            job.append(&label[range.clone()], 0.0, active);
            job.append(&label[range.end..], 0.0, plain);
        }
        None => job.append(label, 0.0, plain),
    }
    egui::Area::new(Id::new(("signature", tab)))
        .order(egui::Order::Foreground)
        .fixed_pos(anchor + Vec2::new(-6.0, 4.0))
        .interactable(false)
        .show(ui.ctx(), |ui| {
            egui::Frame::new()
                .fill(palette.surface)
                .stroke(Stroke::new(1.0, palette.outline))
                .corner_radius(CornerRadius::same(6))
                .inner_margin(egui::Margin::symmetric(8, 4))
                .show(ui, |ui| ui.label(job));
        });
}

/// While a selection is dragged near or past the top or bottom of the
/// pane, scrolls towards the pointer, faster the further out it is. The
/// text edit only scrolls when the selection changes, which stops as soon
/// as the pointer holds still.
fn autoscroll(ui: &Ui, response: &egui::Response) {
    /// How close to an edge the pointer starts scrolling.
    const EDGE: f32 = 24.0;
    if !response.dragged() {
        return;
    }
    let Some(pointer) = ui.ctx().pointer_latest_pos() else {
        return;
    };
    let view = ui.clip_rect();
    let above = view.top() + EDGE - pointer.y;
    let below = pointer.y - (view.bottom() - EDGE);
    // Positive moves the content down, showing what is above.
    let delta = if above > 0.0 {
        above
    } else if below > 0.0 {
        -below
    } else {
        return;
    };
    let step = delta.signum() * (delta.abs() * 0.5).clamp(2.0, 60.0);
    ui.scroll_with_delta_animation(egui::vec2(0.0, step), egui::style::ScrollAnimation::none());
    ui.ctx().request_repaint();
}

/// Opens, keeps or closes the list after the editor had its turn.
fn follow(
    popup: &mut Popup,
    sql: &str,
    selection: &std::ops::Range<usize>,
    changed: bool,
    summoned: bool,
    focused: bool,
    language: Language,
) {
    let cursor = selection.start;
    if !focused || !selection.is_empty() {
        popup.close();
        return;
    }
    if changed {
        // Typing a name or a dot opens it, anything else closes it.
        let typed = sql[..cursor.min(sql.len())].chars().next_back();
        // Redis words go on through `:`, `-` and `.`, as in `user:1:name`.
        let word = |c: char| {
            if language == Language::Redis {
                completion::redis_word(c)
            } else {
                c.is_alphanumeric() || c == '_' || c == '.'
            }
        };
        // SQL also opens it where a table or column is sure to follow.
        let sql_slot = !matches!(language, Language::Redis | Language::Mongodb)
            && completion::opens_by_itself(sql, cursor);
        if typed.is_some_and(word) || sql_slot {
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
    let stale = popup.completion.as_ref().is_none_or(|completion| {
        popup.cursor != cursor
            || popup.connection != assist.connection
            || completion.needs_answered(assist.catalog, assist.keys.as_ref())
    });
    if !stale {
        // Still waiting on the catalog: ask again for what is missing, which
        // costs nothing where it is loading, but do not make the list again.
        if let (Some(connection), Some(completion)) = (assist.connection, &popup.completion)
            && !completion.needs.is_empty()
        {
            actions.push(Action::LoadForCompletion(
                connection,
                completion.needs.clone(),
            ));
        }
        return;
    }
    let fresh = popup.completion.is_none()
        || popup.cursor != cursor
        || popup.connection != assist.connection;
    popup.cursor = cursor;
    popup.connection = assist.connection;
    let completed = match (assist.language, assist.keys) {
        (Language::Redis, _) => {
            completion::complete_redis(sql, cursor, assist.keys.as_ref(), assist.options.history)
        }
        (Language::Mongodb, _) => {
            let schema = assist.options.search.first().map(String::as_str);
            let collections = schema.and_then(|schema| assist.catalog.objects(schema));
            completion::complete_mongo(sql, cursor, schema, collections)
        }
        _ => completion::complete(sql, cursor, assist.catalog, &assist.options),
    };
    let Some(completion) = completed else {
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

/// Remembers an accepted item for its connection's ranking.
fn accepted(assist: &Assist<'_>, item: Option<Item>, actions: &mut Vec<Action>) {
    if let (Some(connection), Some(item)) = (assist.connection, item) {
        actions.push(Action::CompletionAccepted(connection, item));
    }
}

/// Replaces the word at the cursor with the `index`th item, and returns it.
fn insert(
    ctx: &egui::Context,
    text_id: Id,
    sql: &mut String,
    popup: &mut Popup,
    snippet: &mut Option<Snippet>,
    index: usize,
) -> Option<Item> {
    let completion = popup.completion.take()?;
    popup.close();
    let item = completion.items.get(index)?;
    let range = completion.range.start.min(sql.len())..completion.range.end.min(sql.len());
    let (stops, caret) = place(sql, range, item);
    *snippet = stops;
    select(ctx, text_id, sql, caret);
    Some(item.clone())
}

/// Replaces `range` of `sql` with `item`, and returns the stops Tab visits
/// after it and the byte range to select: the caret, or a placeholder.
fn place(sql: &mut String, range: Range<usize>, item: &Item) -> (Option<Snippet>, Range<usize>) {
    sql.replace_range(range.clone(), &item.insert);
    // `caret` and `stops` are offsets into the inserted text.
    let stops = Snippet::new(range.start, &item.stops, sql.len());
    let caret = range.start + item.caret;
    (stops, caret..caret + item.caret_len)
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
        Kind::Key => (Icon::Key, palette.accent),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether the list is open after typing the end of `sql`.
    fn opened(sql: &str, language: Language) -> bool {
        let mut popup = Popup::default();
        let end = sql.len();
        follow(&mut popup, sql, &(end..end), true, false, true, language);
        popup.open
    }

    #[test]
    fn typing_into_a_table_slot_opens_the_list() {
        assert!(opened("SELECT * FROM ", Language::Sql));
        assert!(opened("INSERT INTO orders (", Language::Sql));
        assert!(!opened("SELECT a, ", Language::Sql));
        assert!(!opened("SELECT ' FROM ", Language::Sql));
        assert!(!opened("FROM ", Language::Redis));
        assert!(!opened("db.find({ FROM ", Language::Mongodb));
        assert!(opened("db.", Language::Mongodb));
    }

    #[test]
    fn tab_moves_through_pending_stops_before_it_takes_a_list_item() {
        assert_eq!(tab_picks(true, false), Tab::Item);
        assert_eq!(tab_picks(true, true), Tab::Stop);
        assert_eq!(tab_picks(false, true), Tab::Stop);
        assert_eq!(tab_picks(false, false), Tab::Plain);
    }

    #[test]
    fn marked_problems_are_underlined_in_their_severity() {
        let palette = Palette::DARK;
        let text = "SELECT a FROM nope";
        let marks = vec![(14..18, Severity::Warning)];
        let job = highlight(text, &palette, None, Language::Sql, &marks);
        let underlined: String = job
            .sections
            .iter()
            .filter(|section| section.format.underline != Stroke::NONE)
            .map(|section| &text[section.byte_range.start.0..section.byte_range.end.0])
            .collect();
        assert_eq!(underlined, "nope");
        let color = job
            .sections
            .iter()
            .find(|section| section.format.underline != Stroke::NONE)
            .map(|section| section.format.underline.color);
        assert_eq!(color, Some(palette.warning));
        // A server error under it wins.
        let job = highlight(text, &palette, Some(15), Language::Sql, &marks);
        assert!(
            job.sections
                .iter()
                .any(|section| section.format.underline.color == palette.danger)
        );
    }

    /// One table, `t(a, b, c)`, in `public`.
    struct Table {
        schemas: Vec<String>,
        objects: Vec<DbObject>,
        columns: Vec<ColumnInfo>,
    }

    impl Table {
        fn new() -> Self {
            let column = |name: &str| ColumnInfo {
                name: name.into(),
                data_type: "integer".into(),
                nullable: true,
                default: None,
                primary_key: false,
                comment: None,
            };
            Self {
                schemas: vec!["public".into()],
                objects: vec![DbObject {
                    schema: "public".into(),
                    name: "t".into(),
                    kind: violet_core::ObjectKind::Table,
                    comment: None,
                    estimated_rows: None,
                    subtype: None,
                    detail: None,
                }],
                columns: vec![column("a"), column("b"), column("c")],
            }
        }
    }

    impl Catalog for Table {
        fn schemas(&self) -> &[String] {
            &self.schemas
        }
        fn objects(&self, _: &str) -> Option<&[DbObject]> {
            Some(&self.objects)
        }
        fn columns(&self, _: &str, _: &str) -> Option<&[ColumnInfo]> {
            Some(&self.columns)
        }
        fn foreign_keys(&self, _: &str) -> Option<&[ForeignKey]> {
            None
        }
    }

    #[test]
    fn an_inserted_template_keeps_every_stop_at_its_place() {
        let catalog = Table::new();
        let history = History::default();
        let search = vec!["public".to_owned()];
        let quote = |name: &str| name.to_owned();
        let options = Options {
            search: &search,
            database: None,
            quote: &quote,
            family: Family::Other,
            case: KeywordCase::default(),
            history: &history,
        };
        let mut sql = "INSERT INTO t ".to_owned();
        let completion =
            completion::complete(&sql, sql.len(), &catalog, &options).expect("completion");
        let item = completion
            .items
            .iter()
            .find(|item| item.detail == "template")
            .expect("template");
        let (stops, caret) = place(&mut sql, completion.range.clone(), item);
        assert_eq!(&sql[..caret.start], "INSERT INTO t (a, b, c) VALUES (");
        let mut stops = stops.expect("stops");
        // Each Tab lands at the next value, not shifted by the caret.
        let second = stops.next(caret.start).expect("second");
        assert_eq!(&sql[..second.start], "INSERT INTO t (a, b, c) VALUES (, ");
        let third = stops.next(second.start).expect("third");
        assert_eq!(&sql[..third.start], "INSERT INTO t (a, b, c) VALUES (, , ");
        assert!(stops.is_empty());
    }

    #[test]
    fn a_function_placeholder_is_typed_over_and_tab_moves_on() {
        let item = |sql: &str, label: &str| {
            let catalog = Table::new();
            let history = History::default();
            let search = vec!["public".to_owned()];
            let quote = |name: &str| name.to_owned();
            let options = Options {
                search: &search,
                database: None,
                quote: &quote,
                family: Family::Other,
                case: KeywordCase::default(),
                history: &history,
            };
            let completion =
                completion::complete(sql, sql.len(), &catalog, &options).expect("completion");
            let found = completion
                .items
                .iter()
                .find(|item| item.label == label)
                .expect("item")
                .clone();
            (completion.range, found)
        };
        let mut sql = "SELECT rou".to_owned();
        let (range, round) = item(&sql, "round()");
        let (stops, first) = place(&mut sql, range, &round);
        assert_eq!(sql, "SELECT round(number, decimals)");
        assert_eq!(&sql[first.clone()], "number");
        let mut stops = stops.expect("stops");
        // Typing over the placeholder moves the later ones along.
        sql.replace_range(first.clone(), "price");
        let cursor = first.start + "price".len();
        assert!(stops.follow(sql.len(), cursor));
        let second = stops.next(cursor).expect("second");
        assert_eq!(&sql[second.clone()], "decimals");
        sql.replace_range(second.clone(), "2");
        assert!(stops.follow(sql.len(), second.start + 1));
        let end = stops.next(second.start + 1).expect("end");
        assert_eq!(end, sql.len()..sql.len());
        assert!(stops.is_empty());
    }

    use std::cell::Cell;

    use crate::completion::{History, Need, Options};
    use crate::dialect::Family;
    use crate::settings::KeywordCase;
    use violet_core::{ColumnInfo, DbObject, ForeignKey};

    /// A catalog with `public` loaded or not, counting how often it is read.
    struct Counting {
        loaded: bool,
        reads: Cell<usize>,
    }

    impl Catalog for Counting {
        fn schemas(&self) -> &[String] {
            self.reads.set(self.reads.get() + 1);
            &[]
        }
        fn objects(&self, _: &str) -> Option<&[DbObject]> {
            self.loaded.then_some(&[][..])
        }
        fn columns(&self, _: &str, _: &str) -> Option<&[ColumnInfo]> {
            None
        }
        fn foreign_keys(&self, _: &str) -> Option<&[ForeignKey]> {
            None
        }
    }

    #[test]
    fn a_list_waiting_on_the_catalog_is_not_made_again_every_frame() {
        let history = History::default();
        let search = vec!["public".to_owned()];
        let quote = |name: &str| name.to_owned();
        let options = Options {
            search: &search,
            database: None,
            quote: &quote,
            family: Family::Other,
            case: KeywordCase::default(),
            history: &history,
        };
        let connection = Some(ConnectionId::new());
        let sql = "SELECT * FROM ";
        let mut popup = Popup::default();
        let mut actions = Vec::new();
        let show = |popup: &mut Popup, catalog: &Counting, actions: &mut Vec<Action>| {
            let assist = Assist {
                connection,
                language: Language::Sql,
                catalog,
                options: Options { ..options },
                keys: None,
            };
            refresh(popup, sql, sql.len(), &assist, actions);
        };
        let waiting = Counting {
            loaded: false,
            reads: Cell::new(0),
        };
        show(&mut popup, &waiting, &mut actions);
        let first = waiting.reads.get();
        assert!(first > 0);
        assert_eq!(actions.len(), 1);
        // Frames while it loads ask again but do not read the catalog.
        show(&mut popup, &waiting, &mut actions);
        show(&mut popup, &waiting, &mut actions);
        assert_eq!(waiting.reads.get(), first);
        assert_eq!(actions.len(), 3);
        assert!(matches!(
            &actions[2],
            Action::LoadForCompletion(_, needs) if needs == &[Need::Objects("public".into())]
        ));
        // Once it arrives the list is made again.
        let loaded = Counting {
            loaded: true,
            reads: Cell::new(0),
        };
        show(&mut popup, &loaded, &mut actions);
        assert!(loaded.reads.get() > 0);
        // Nothing is left to wait for, and nothing to list.
        assert_eq!(actions.len(), 3);
        assert!(popup.completion.is_none());
    }
}
