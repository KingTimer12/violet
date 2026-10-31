//! The earlier versions of a saved script: a list of them, what changed
//! from each to the script as it is now, and a way to put one back in
//! the editor.

use std::path::PathBuf;

use egui::{Align2, CornerRadius, RichText, ScrollArea, Sense, Stroke, Vec2};
use violet_storage::{Performance, PlanSummary, Revision, RevisionNote, RunSummary};

use super::VioletApp;
use super::action::Action;
use super::icons::Icon;
use super::theme::{self, Palette};
use super::toasts::Toast;
use super::widgets::{self, Tone};
use super::workspace::{Outcome, QueryTab, TabKind};

/// The most lines compared line by line; past it the whole text counts as
/// changed.
const DIFF_CELLS: usize = 1_000_000;

/// Unchanged lines kept around a change.
const CONTEXT: usize = 2;

const ROW_HEIGHT: f32 = 20.0;
const GLYPH_WIDTH: f32 = 7.7;
const GUTTER: f32 = 30.0;

/// Statement text kept with a run, in characters.
const STATEMENT_CHARS: usize = 200;

/// The steps of a plan kept with a version.
const HEAVIEST: usize = 3;

/// A script's history dialog.
pub struct History {
    path: PathBuf,
    name: String,
    versions: Vec<Version>,
    selected: usize,
    view: View,
    /// The version whose comment was typed in and not kept yet.
    dirty: Option<usize>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Changes,
    Full,
}

/// A saved version, compared with the script as it is now.
struct Version {
    revision: Revision,
    /// The comment, and how the script ran when it was saved.
    note: RevisionNote,
    added: usize,
    removed: usize,
    /// Only what changed, with a little around it.
    changes: Vec<Row>,
    full: Vec<Row>,
    /// The longest line, in characters.
    widest: usize,
    /// The day it was saved: `Today`, `Yesterday` or a date.
    day: String,
    /// The time of day, to the second.
    time: String,
}

enum Row {
    Line(Change, String),
    /// Unchanged lines left out.
    Gap(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Change {
    Same,
    Removed,
    Added,
}

impl VioletApp {
    /// Opens the history of the script at `path`.
    pub(super) fn show_script_history(&mut self, ctx: &egui::Context, path: PathBuf) {
        let revisions = match self.scripts.revisions(&path) {
            Ok(revisions) => revisions,
            Err(error) => {
                self.toasts.push(
                    ctx,
                    Toast::error("Could not read the history", error.to_string()),
                );
                return;
            }
        };
        // The editor's text when it is open, else the file's.
        let current = self
            .workspace
            .script_tab(&path)
            .and_then(|id| self.workspace.get(id))
            .map(|tab| tab.sql.clone())
            .or_else(|| violet_storage::scripts::read(&path).ok())
            .unwrap_or_default();
        let now = jiff::Zoned::now();
        let versions: Vec<Version> = revisions
            .into_iter()
            .filter_map(|revision| {
                let text = self.scripts.read_revision(&path, revision.id).ok()?;
                let note = self.scripts.revision_note(&path, revision.id);
                Some(Version::new(revision, note, &text, &current, &now))
            })
            .collect();
        let name = super::script_flow::script_title(&self.scripts, &path);
        // The newest save is often the script as it is: start on the
        // first one that shows something.
        let selected = versions
            .iter()
            .position(|version| version.added + version.removed > 0)
            .unwrap_or(0);
        self.script_history = Some(History {
            path,
            name,
            versions,
            selected,
            view: View::Changes,
            dirty: None,
        });
    }

    /// Puts a saved version in the script's editor, opening it first; it
    /// stays unsaved until saved.
    pub(super) fn restore_revision(&mut self, ctx: &egui::Context, path: PathBuf, id: u64) {
        let text = match self.scripts.read_revision(&path, id) {
            Ok(text) => text,
            Err(error) => {
                self.toasts.push(
                    ctx,
                    Toast::error("Could not restore the version", error.to_string()),
                );
                return;
            }
        };
        self.open_script(ctx, path.clone());
        let Some(tab) = self
            .workspace
            .script_tab(&path)
            .and_then(|id| self.workspace.get_mut(id))
        else {
            return;
        };
        if !matches!(tab.kind, TabKind::Script) {
            return;
        }
        tab.sql = text;
        let end = tab.sql.len();
        tab.selection = end..end;
        let caret = tab.sql.chars().count();
        super::editor::place_cursor(ctx, tab.id, caret);
        self.toasts.push(
            ctx,
            Toast::success("Version restored. Save the script to keep it."),
        );
    }
}

impl Version {
    fn new(
        revision: Revision,
        note: RevisionNote,
        text: &str,
        current: &str,
        now: &jiff::Zoned,
    ) -> Self {
        let (day, time) = stamp(revision.id, now);
        let diff = line_diff(text, current);
        let count = |wanted| diff.iter().filter(|(change, _)| *change == wanted).count();
        let (added, removed) = (count(Change::Added), count(Change::Removed));
        let near_change: Vec<bool> = (0..diff.len())
            .map(|at| {
                let from = at.saturating_sub(CONTEXT);
                let to = (at + CONTEXT + 1).min(diff.len());
                diff[from..to]
                    .iter()
                    .any(|(change, _)| *change != Change::Same)
            })
            .collect();
        let mut changes = Vec::new();
        let mut left_out = 0;
        for ((change, line), keep) in diff.iter().zip(&near_change) {
            if *keep {
                if left_out > 0 {
                    changes.push(Row::Gap(left_out));
                    left_out = 0;
                }
                changes.push(Row::Line(*change, line.clone()));
            } else {
                left_out += 1;
            }
        }
        if left_out > 0 && !changes.is_empty() {
            changes.push(Row::Gap(left_out));
        }
        let widest = diff
            .iter()
            .map(|(_, line)| line.chars().count())
            .max()
            .unwrap_or(0);
        Self {
            revision,
            note,
            added,
            removed,
            changes,
            full: text
                .lines()
                .map(|line| Row::Line(Change::Same, line.to_owned()))
                .collect(),
            widest,
            day,
            time,
        }
    }
}

/// Draws the dialog. Returns `false` once it should close.
pub fn show(ctx: &egui::Context, history: &mut History, actions: &mut Vec<Action>) -> bool {
    let palette = Palette::current(ctx);
    let mut open = true;
    let modal = egui::Modal::new(egui::Id::new("script-history"))
        .frame(
            egui::Frame::new()
                .fill(palette.surface)
                .stroke(Stroke::new(1.0, palette.outline))
                .corner_radius(CornerRadius::same(14))
                .shadow(ctx.global_style().visuals.window_shadow)
                .inner_margin(egui::Margin::same(24)),
        )
        .show(ctx, |ui| {
            ui.set_width(860.0_f32.min(ui.ctx().content_rect().width() - 90.0));
            ui.spacing_mut().item_spacing.y = 14.0;
            header(ui, &palette, history, &mut open);
            if history.versions.is_empty() {
                ui.add_space(8.0);
                widgets::empty_state(
                    ui,
                    Icon::Clock,
                    "No earlier versions yet",
                    "Each time you save the script, a version is kept here.",
                );
                ui.add_space(24.0);
            } else {
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = 16.0;
                    versions(ui, &palette, history);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 10.0;
                        ui.set_width(ui.available_width());
                        preview(ui, &palette, history, actions);
                    });
                });
            }
            footer(ui, &palette, history, actions, &mut open);
        });
    let keep_open = open && !modal.should_close();
    if let Some(index) = history.dirty
        && (!keep_open || index != history.selected)
    {
        keep_comment(history, index, actions);
    }
    keep_open
}

/// Asks for the comment typed for the version at `index` to be kept.
fn keep_comment(history: &mut History, index: usize, actions: &mut Vec<Action>) {
    history.dirty = None;
    if let Some(version) = history.versions.get(index) {
        actions.push(Action::SetRevisionComment(
            history.path.clone(),
            version.revision.id,
            version.note.comment.clone(),
        ));
    }
}

fn header(ui: &mut egui::Ui, palette: &Palette, history: &History, open: &mut bool) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(36.0), Sense::hover());
        ui.painter()
            .rect_filled(rect, CornerRadius::same(10), palette.accent_soft);
        widgets::paint_icon(ui, Icon::Clock, rect.shrink(9.0), palette.accent);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.label(
                RichText::new(format!("History · {}", history.name))
                    .font(theme::semibold(17.0))
                    .color(palette.text),
            );
            let count = history.versions.len();
            ui.label(
                RichText::new(format!(
                    "{count} saved version{}, the newest {} are kept",
                    if count == 1 { "" } else { "s" },
                    violet_storage::REVISIONS_KEPT
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

/// The saved versions, newest first, on a timeline: a rail with a dot
/// for each, grouped by day.
fn versions(ui: &mut egui::Ui, palette: &Palette, history: &mut History) {
    const WIDTH: f32 = 264.0;
    const ROW: f32 = 36.0;
    const TALL_ROW: f32 = 52.0;
    const RAIL: f32 = 14.0;
    ui.vertical(|ui| {
        ScrollArea::vertical()
            .id_salt("script-history-versions")
            .max_height(diff_height(ui.ctx()) + 220.0)
            .auto_shrink([true, false])
            .show(ui, |ui| {
                ui.set_width(WIDTH);
                ui.spacing_mut().item_spacing.y = 0.0;
                let last = history.versions.len().saturating_sub(1);
                let mut group = "";
                for index in 0..history.versions.len() {
                    let version = &history.versions[index];
                    let age = version.day.as_str();
                    if age != group {
                        group = age;
                        ui.add_space(if index == 0 { 0.0 } else { 10.0 });
                        widgets::caption(ui, age);
                        ui.add_space(4.0);
                    }
                    let second = second_line(&version.note);
                    let (rect, response) = ui.allocate_exact_size(
                        Vec2::new(WIDTH, if second.is_some() { TALL_ROW } else { ROW }),
                        Sense::click(),
                    );
                    let top_line = if second.is_some() {
                        rect.top() + 18.0
                    } else {
                        rect.center().y
                    };
                    let selected = index == history.selected;
                    let painter = ui.painter();
                    if selected {
                        painter.rect_filled(rect, CornerRadius::same(8), palette.accent_soft);
                    } else if response.hovered() {
                        painter.rect_filled(rect, CornerRadius::same(8), palette.surface_hover);
                    }
                    let rail = rect.left() + RAIL;
                    let (top, bottom) = (
                        if index == 0 {
                            rect.center().y
                        } else {
                            rect.top()
                        },
                        if index == last {
                            rect.center().y
                        } else {
                            rect.bottom()
                        },
                    );
                    painter.line_segment(
                        [egui::pos2(rail, top), egui::pos2(rail, bottom)],
                        Stroke::new(1.5, palette.outline),
                    );
                    let dot = egui::pos2(rail, rect.center().y);
                    if selected {
                        painter.circle_filled(dot, 5.0, palette.accent);
                    } else {
                        painter.circle_filled(dot, 4.0, palette.surface);
                        painter.circle_stroke(dot, 4.0, Stroke::new(1.5, palette.dim));
                    }
                    let when = painter.text(
                        egui::pos2(rail + 16.0, top_line),
                        Align2::LEFT_CENTER,
                        version.time.clone(),
                        theme::semibold(13.0),
                        palette.text,
                    );
                    painter.text(
                        egui::pos2(when.right() + 7.0, top_line),
                        Align2::LEFT_CENTER,
                        format!("#{}", last + 1 - index),
                        theme::medium(11.0),
                        palette.dim,
                    );
                    let right = rect.right() - 10.0;
                    if version.added + version.removed == 0 {
                        painter.text(
                            egui::pos2(right, top_line),
                            Align2::RIGHT_CENTER,
                            "same",
                            theme::medium(11.5),
                            palette.dim,
                        );
                    } else {
                        let removed = painter.text(
                            egui::pos2(right, top_line),
                            Align2::RIGHT_CENTER,
                            format!("\u{2212}{}", version.removed),
                            theme::mono(11.5),
                            palette.danger,
                        );
                        painter.text(
                            egui::pos2(removed.left() - 7.0, top_line),
                            Align2::RIGHT_CENTER,
                            format!("+{}", version.added),
                            theme::mono(11.5),
                            palette.success,
                        );
                    }
                    if let Some((text, is_comment)) = second {
                        let color = if is_comment {
                            palette.secondary
                        } else {
                            palette.dim
                        };
                        let galley = widgets::elided(
                            ui,
                            &text,
                            theme::medium(11.5),
                            color,
                            WIDTH - RAIL - 16.0 - 10.0,
                        );
                        ui.painter().galley(
                            egui::pos2(rail + 16.0, rect.bottom() - 17.0 - galley.size().y / 2.0),
                            galley,
                            color,
                        );
                    }
                    if response
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .clicked()
                    {
                        history.selected = index;
                    }
                }
            });
    });
}

/// What the selected version holds, as a diff or whole.
fn preview(ui: &mut egui::Ui, palette: &Palette, history: &mut History, actions: &mut Vec<Action>) {
    let selected = history.selected;
    if let Some(version) = history.versions.get_mut(selected) {
        let comment = ui.add(
            egui::TextEdit::multiline(&mut version.note.comment)
                .id(egui::Id::new("script-history-comment"))
                .hint_text("Comment on this version: what changed, why")
                .desired_rows(2)
                .margin(egui::vec2(10.0, 7.0))
                .desired_width(f32::INFINITY),
        );
        if comment.changed() {
            history.dirty = Some(selected);
        }
        if comment.lost_focus() && history.dirty.is_some() {
            keep_comment(history, selected, actions);
        }
    }
    if let Some(performance) = history
        .versions
        .get(selected)
        .map(|version| &version.note.performance)
    {
        performance_card(ui, palette, performance.as_ref());
    }
    ui.horizontal(|ui| {
        widgets::segmented(
            ui,
            &mut history.view,
            &[(View::Changes, "Changes"), (View::Full, "Full version")],
        );
        ui.add_space(4.0);
        let note = match history.view {
            View::Changes => "Red: in this version. Green: in the script now.",
            View::Full => "This version, as it was saved.",
        };
        ui.label(RichText::new(note).color(palette.dim));
    });
    let Some(version) = history.versions.get(history.selected) else {
        return;
    };
    let rows = match history.view {
        View::Changes => &version.changes,
        View::Full => &version.full,
    };
    egui::Frame::new()
        .fill(palette.window)
        .stroke(Stroke::new(1.0, palette.outline))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(egui::Margin::same(1))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.set_height(diff_height(ui.ctx()));
            if rows.is_empty() {
                ui.add_space(diff_height(ui.ctx()) / 2.0 - 30.0);
                ui.vertical_centered(|ui| {
                    let text = match history.view {
                        View::Changes => "Identical to the script now.",
                        View::Full => "This version is empty.",
                    };
                    ui.label(RichText::new(text).color(palette.dim));
                });
                return;
            }
            let available = ui.available_width();
            let width = (version.widest as f32 * GLYPH_WIDTH + GUTTER + 24.0).max(available);
            ScrollArea::both()
                .id_salt("script-history-diff")
                .auto_shrink([false, false])
                .show_rows(ui, ROW_HEIGHT, rows.len(), |ui, range| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    for row in &rows[range] {
                        paint_row(ui, palette, row, width);
                    }
                });
        });
}

fn paint_row(ui: &mut egui::Ui, palette: &Palette, row: &Row, width: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, ROW_HEIGHT), Sense::hover());
    let painter = ui.painter();
    let font = theme::mono(12.5);
    let middle = rect.left_center();
    match row {
        Row::Gap(count) => {
            painter.rect_filled(rect, 0.0, palette.panel);
            painter.text(
                middle + Vec2::new(GUTTER, 0.0),
                Align2::LEFT_CENTER,
                format!(
                    "{count} unchanged line{}",
                    if *count == 1 { "" } else { "s" }
                ),
                theme::medium(11.5),
                palette.dim,
            );
        }
        Row::Line(change, line) => {
            let (sign, color, tint) = match change {
                Change::Same => ("", palette.text, None),
                Change::Removed => ("\u{2212}", palette.danger, Some(palette.danger)),
                Change::Added => ("+", palette.success, Some(palette.success)),
            };
            if let Some(tint) = tint {
                painter.rect_filled(rect, 0.0, tint.gamma_multiply(0.14));
            }
            painter.text(
                middle + Vec2::new(11.0, 0.0),
                Align2::LEFT_CENTER,
                sign,
                font.clone(),
                color,
            );
            painter.text(
                middle + Vec2::new(GUTTER, 0.0),
                Align2::LEFT_CENTER,
                line,
                font,
                color,
            );
        }
    }
}

fn footer(
    ui: &mut egui::Ui,
    palette: &Palette,
    history: &History,
    actions: &mut Vec<Action>,
    open: &mut bool,
) {
    ui.horizontal(|ui| {
        ui.add(
            egui::Label::new(
                RichText::new(
                    "Restoring puts the version in the editor. Nothing is saved until you save.",
                )
                .size(11.5)
                .color(palette.dim),
            )
            .wrap(),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if let Some(version) = history.versions.get(history.selected)
                && widgets::button(ui, Tone::Primary, Some(Icon::Undo), "Restore this version")
                    .clicked()
            {
                actions.push(Action::RestoreRevision(
                    history.path.clone(),
                    version.revision.id,
                ));
                *open = false;
            }
            if widgets::button(ui, Tone::Ghost, None, "Close").clicked() {
                *open = false;
            }
        });
    });
}

/// The line under a version's time in the list: its comment, else how
/// the script ran. `true` for a comment.
fn second_line(note: &RevisionNote) -> Option<(String, bool)> {
    if let Some(first) = note.comment.lines().find(|line| !line.trim().is_empty()) {
        return Some((first.trim().to_owned(), true));
    }
    let run = note.performance.as_ref()?.run.as_ref()?;
    let mut text = format_ms(run.elapsed_ms);
    if let Some(rows) = run.rows {
        text.push_str(&format!(
            " \u{B7} {} rows",
            super::workspace::thousands(rows as usize)
        ));
    }
    Some((text, false))
}

/// `0.8 ms`, `42 ms`, `1.25 s`.
fn format_ms(ms: f64) -> String {
    if ms >= 1000.0 {
        format!("{:.2} s", ms / 1000.0)
    } else if ms >= 10.0 {
        format!("{ms:.0} ms")
    } else {
        format!("{ms:.1} ms")
    }
}

/// How the script ran when the version was saved.
fn performance_card(ui: &mut egui::Ui, palette: &Palette, performance: Option<&Performance>) {
    egui::Frame::new()
        .fill(palette.window)
        .stroke(Stroke::new(1.0, palette.outline))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(egui::Margin::symmetric(12, 9))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 4.0;
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(14.0), Sense::hover());
                widgets::paint_icon(ui, Icon::Activity, rect, palette.accent);
                ui.label(
                    RichText::new("Performance when saved")
                        .font(theme::semibold(12.5))
                        .color(palette.text),
                );
            });
            let Some(performance) = performance else {
                ui.label(
                    RichText::new(
                        "Nothing was measured. Run the script before saving to keep its timing.",
                    )
                    .color(palette.dim),
                );
                return;
            };
            if let Some(run) = &performance.run {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    ui.label(RichText::new("Run").color(palette.secondary));
                    ui.label(
                        RichText::new(format_ms(run.elapsed_ms))
                            .font(theme::semibold(12.5))
                            .color(palette.text),
                    );
                    if let Some(rows) = run.rows {
                        ui.label(
                            RichText::new(format!(
                                "{} row{}",
                                super::workspace::thousands(rows as usize),
                                if rows == 1 { "" } else { "s" }
                            ))
                            .color(palette.secondary),
                        );
                    }
                    ui.add(
                        egui::Label::new(
                            RichText::new(statement_line(&run.statement))
                                .font(theme::mono(11.5))
                                .color(palette.dim),
                        )
                        .truncate(),
                    );
                });
            }
            if let Some(plan) = &performance.plan {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    ui.label(RichText::new("Plan").color(palette.secondary));
                    for (label, value) in [
                        ("planning", plan.planning_ms),
                        ("execution", plan.execution_ms),
                    ] {
                        if let Some(ms) = value {
                            ui.label(
                                RichText::new(format!("{label} {}", format_ms(ms)))
                                    .font(theme::semibold(12.5))
                                    .color(palette.text),
                            );
                        }
                    }
                    for warning in &plan.warnings {
                        widgets::pill(
                            ui,
                            warning,
                            palette.warning,
                            palette.warning.gamma_multiply(0.16),
                        );
                    }
                });
                for step in &plan.heaviest {
                    ui.add(
                        egui::Label::new(
                            RichText::new(format!("  {step}"))
                                .font(theme::mono(11.5))
                                .color(palette.secondary),
                        )
                        .truncate(),
                    );
                }
            }
        });
}

/// A statement on one line.
fn statement_line(statement: &str) -> String {
    statement.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// How `tab`'s script ran: its last run and last plan, kept only while
/// the script still holds the statement that ran.
pub(super) fn performance_of(tab: &QueryTab) -> Option<Performance> {
    let (executed, _) = tab.last_run.as_ref()?;
    let ran: String = executed.sql.chars().skip(executed.shift).collect();
    let ran = ran.trim();
    if ran.is_empty() || !tab.sql.contains(ran) {
        return None;
    }
    let explain = crate::plan::is_explain(&executed.sql);
    let run = match &tab.outcome {
        Some(Outcome::Done { output }) => Some(RunSummary {
            statement: shorten(ran),
            elapsed_ms: output.elapsed.as_secs_f64() * 1000.0,
            rows: if explain {
                None
            } else {
                output.sets.first().map(|set| {
                    if set.rows.is_empty() {
                        set.affected.unwrap_or(0)
                    } else {
                        set.rows.len() as u64
                    }
                })
            },
        }),
        _ => None,
    };
    let plan = tab
        .plan_runs
        .last()
        .map(|run| summarize_plan(&run.sql, &run.plan));
    (run.is_some() || plan.is_some()).then_some(Performance { run, plan })
}

fn summarize_plan(sql: &str, plan: &crate::plan::Plan) -> PlanSummary {
    let mut steps = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut pending: Vec<&crate::plan::PlanNode> = plan.roots.iter().collect();
    while let Some(node) = pending.pop() {
        steps.push(node);
        for warning in &node.warnings {
            let label = warning.label().to_owned();
            if !warnings.contains(&label) {
                warnings.push(label);
            }
        }
        pending.extend(&node.children);
    }
    steps.sort_by(|a, b| b.weight().total_cmp(&a.weight()));
    let heaviest = steps
        .into_iter()
        .filter(|node| node.weight() > 0.0)
        .take(HEAVIEST)
        .map(|node| {
            let mut line = node.label.clone();
            if let Some(target) = &node.target {
                line.push_str(&format!(" on {target}"));
            }
            match (node.time_ms, node.cost) {
                (Some(ms), _) => line.push_str(&format!(" \u{B7} {}", format_ms(ms))),
                (None, Some(cost)) => line.push_str(&format!(" \u{B7} cost {cost:.0}")),
                (None, None) => {}
            }
            line
        })
        .collect();
    PlanSummary {
        statement: shorten(sql.trim()),
        planning_ms: plan.planning_ms,
        execution_ms: plan.execution_ms,
        heaviest,
        warnings,
    }
}

fn shorten(statement: &str) -> String {
    let mut short: String = statement.chars().take(STATEMENT_CHARS).collect();
    if statement.chars().count() > STATEMENT_CHARS {
        short.push('\u{2026}');
    }
    short
}

/// The day and the time of day a version was saved, in the local time
/// zone: (`Today`, `14:03:22`).
fn stamp(at: u64, now: &jiff::Zoned) -> (String, String) {
    let Ok(timestamp) = jiff::Timestamp::from_millisecond(i64::try_from(at).unwrap_or(i64::MAX))
    else {
        return (String::new(), String::new());
    };
    let time = timestamp.to_zoned(now.time_zone().clone());
    let (date, today) = (time.date(), now.date());
    let day = if date == today {
        "Today".to_owned()
    } else if today.yesterday().is_ok_and(|yesterday| yesterday == date) {
        "Yesterday".to_owned()
    } else if date.year() == today.year() {
        time.strftime("%b %-d").to_string()
    } else {
        time.strftime("%b %-d, %Y").to_string()
    };
    (day, time.strftime("%H:%M:%S").to_string())
}

/// The lines of `old` and `new` side by side: what only `old` has, what
/// only `new` has, and what both do.
fn line_diff(old: &str, new: &str) -> Vec<(Change, String)> {
    let old: Vec<&str> = old.lines().collect();
    let new: Vec<&str> = new.lines().collect();
    let head = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let tail = old[head..]
        .iter()
        .rev()
        .zip(new[head..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let (old_mid, new_mid) = (&old[head..old.len() - tail], &new[head..new.len() - tail]);

    let mut out: Vec<(Change, String)> = old[..head]
        .iter()
        .map(|line| (Change::Same, (*line).to_owned()))
        .collect();
    if old_mid.len().saturating_mul(new_mid.len()) > DIFF_CELLS {
        out.extend(
            old_mid
                .iter()
                .map(|line| (Change::Removed, (*line).to_owned())),
        );
        out.extend(
            new_mid
                .iter()
                .map(|line| (Change::Added, (*line).to_owned())),
        );
    } else {
        // `lcs[i][j]`: the longest run in common of `old_mid[i..]` and
        // `new_mid[j..]`.
        let width = new_mid.len() + 1;
        let mut lcs = vec![0u32; (old_mid.len() + 1) * width];
        for i in (0..old_mid.len()).rev() {
            for j in (0..new_mid.len()).rev() {
                lcs[i * width + j] = if old_mid[i] == new_mid[j] {
                    lcs[(i + 1) * width + j + 1] + 1
                } else {
                    lcs[(i + 1) * width + j].max(lcs[i * width + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < old_mid.len() && j < new_mid.len() {
            if old_mid[i] == new_mid[j] {
                out.push((Change::Same, old_mid[i].to_owned()));
                i += 1;
                j += 1;
            } else if lcs[(i + 1) * width + j] >= lcs[i * width + j + 1] {
                out.push((Change::Removed, old_mid[i].to_owned()));
                i += 1;
            } else {
                out.push((Change::Added, new_mid[j].to_owned()));
                j += 1;
            }
        }
        out.extend(
            old_mid[i..]
                .iter()
                .map(|line| (Change::Removed, (*line).to_owned())),
        );
        out.extend(
            new_mid[j..]
                .iter()
                .map(|line| (Change::Added, (*line).to_owned())),
        );
    }
    out.extend(
        old[old.len() - tail..]
            .iter()
            .map(|line| (Change::Same, (*line).to_owned())),
    );
    out
}

/// How tall the diff is: what the window leaves once the rest of the
/// dialog is there.
fn diff_height(ctx: &egui::Context) -> f32 {
    (ctx.content_rect().height() - 560.0).clamp(140.0, 280.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marks(diff: &[(Change, String)]) -> String {
        diff.iter()
            .map(|(change, line)| match change {
                Change::Same => format!(" {line}"),
                Change::Removed => format!("-{line}"),
                Change::Added => format!("+{line}"),
            })
            .collect::<Vec<_>>()
            .join("|")
    }

    #[test]
    fn a_changed_line_is_removed_and_added() {
        let diff = line_diff("a\nb\nc", "a\nx\nc");
        assert_eq!(marks(&diff), " a|-b|+x| c");
    }

    #[test]
    fn lines_only_one_side_has_are_marked() {
        assert_eq!(marks(&line_diff("a\nb", "a")), " a|-b");
        assert_eq!(marks(&line_diff("a", "a\nb")), " a|+b");
        assert_eq!(marks(&line_diff("", "")), "");
    }

    #[test]
    fn a_line_moved_inside_the_middle_is_found() {
        let diff = line_diff("a\nb\nc\nd", "a\nc\nb\nd");
        assert!(diff.iter().any(|(change, _)| *change == Change::Removed));
        assert!(diff.iter().any(|(change, _)| *change == Change::Added));
        assert_eq!(diff.first().map(|(change, _)| *change), Some(Change::Same));
        assert_eq!(diff.last().map(|(change, _)| *change), Some(Change::Same));
    }
}
