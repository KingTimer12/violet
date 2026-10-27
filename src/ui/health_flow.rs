//! The server health panel: figures and tables each database keeps about
//! itself (sessions and locks, slow statements, caches, clients), read on
//! demand and optionally every few seconds, with cancel and terminate for
//! the sessions that can be stopped.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use egui::{Align, CornerRadius, Layout, RichText, Sense, Stroke, Vec2};
use egui_extras::{Column, TableBuilder};
use violet_app::{CachePolicy, ExecOptions};
use violet_core::{ConnectionId, QueryOutput, ResultSet};

use super::VioletApp;
use super::action::Action;
use super::icons::Icon;
use super::theme::{self, Palette};
use super::toasts::Toast;
use super::widgets::{self, Tone};
use crate::health::{self, Level, Section, Server, Table};

/// How often a panel with auto-refresh reads again.
const EVERY: Duration = Duration::from_secs(5);

/// What reading a section came to.
pub enum Loaded {
    Loading,
    Ready {
        sets: Vec<ResultSet>,
        took: Duration,
    },
    Failed(String),
}

/// An end of a session waiting for the user's word.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stop {
    pub id: String,
    pub whole: bool,
}

/// The open panel.
pub struct Health {
    pub connection: ConnectionId,
    pub name: String,
    pub server: Server,
    pub sections: Vec<Section>,
    pub current: usize,
    pub loaded: HashMap<&'static str, Loaded>,
    pub auto: bool,
    pub last: Option<Instant>,
    pub selected: Option<usize>,
    pub confirm: Option<Stop>,
}

impl Health {
    fn new(connection: ConnectionId, name: String, server: Server) -> Self {
        Self {
            connection,
            name,
            server,
            sections: health::sections(server),
            current: 0,
            loaded: HashMap::new(),
            auto: false,
            last: None,
            selected: None,
            confirm: None,
        }
    }

    fn section(&self) -> Option<&Section> {
        self.sections.get(self.current)
    }

    fn busy(&self) -> bool {
        self.loaded
            .values()
            .any(|loaded| matches!(loaded, Loaded::Loading))
    }
}

/// What a request of the panel asked.
pub enum Asked {
    /// A section, and whether its fallback is what ran.
    Section(&'static str, bool),
    /// An end of a session.
    Stop(String),
}

const READ: ExecOptions = ExecOptions {
    cache: CachePolicy::Refresh,
    page: None,
};

impl VioletApp {
    /// Opens the panel for `connection`.
    pub(super) fn open_health(&mut self, ctx: &egui::Context, connection: ConnectionId) {
        if self.wait_for_connection(connection, || Action::OpenHealth(connection)) {
            return;
        }
        let server = Server::of(self.language(connection), self.family(connection));
        let name = self.profile_name(connection);
        if server == Server::Unknown {
            self.toasts.push(
                ctx,
                Toast::info(
                    "Violet does not know this database's catalog, so it has no health panel",
                ),
            );
            return;
        }
        self.health = Some(Health::new(connection, name, server));
        self.refresh_health();
    }

    /// Reads the overview and the section on show.
    pub(super) fn refresh_health(&mut self) {
        let Some(view) = &self.health else { return };
        let mut wanted: Vec<usize> = vec![0];
        if view.current != 0 {
            wanted.push(view.current);
        }
        for index in wanted {
            self.load_section(index);
        }
        if let Some(view) = &mut self.health {
            view.last = Some(Instant::now());
        }
    }

    pub(super) fn load_section(&mut self, index: usize) {
        let Some(view) = &mut self.health else { return };
        let Some(section) = view.sections.get(index) else {
            return;
        };
        let (id, sql, connection) = (section.id, section.sql.clone(), view.connection);
        view.loaded.insert(id, Loaded::Loading);
        let request = self.backend.execute(connection, sql, READ);
        self.health_requests
            .insert(request, Asked::Section(id, false));
    }

    /// An answer to something the panel asked.
    pub(super) fn health_answered(
        &mut self,
        ctx: &egui::Context,
        asked: Asked,
        result: violet_core::Result<QueryOutput>,
    ) {
        match asked {
            Asked::Stop(what) => {
                match result {
                    Ok(_) => self.toasts.push(ctx, Toast::success(what)),
                    Err(error) => self
                        .toasts
                        .push(ctx, Toast::error("Could not stop it", error.message)),
                }
                if let Some(view) = &mut self.health {
                    view.selected = None;
                }
                self.refresh_health();
            }
            Asked::Section(id, tried_fallback) => {
                let Some(view) = &mut self.health else { return };
                let Some(section) = view.sections.iter().find(|s| s.id == id) else {
                    return;
                };
                match result {
                    Ok(output) => {
                        view.loaded.insert(
                            id,
                            Loaded::Ready {
                                sets: output.sets,
                                took: output.elapsed,
                            },
                        );
                    }
                    Err(error) => {
                        if let (false, Some(fallback)) = (tried_fallback, section.fallback.clone())
                        {
                            let request = self.backend.execute(view.connection, fallback, READ);
                            self.health_requests
                                .insert(request, Asked::Section(id, true));
                            return;
                        }
                        let mut message = error.message;
                        if !section.needs.is_empty() {
                            message.push_str("\n\nThis needs: ");
                            message.push_str(section.needs);
                        }
                        view.loaded.insert(id, Loaded::Failed(message));
                    }
                }
            }
        }
    }

    /// Ends a session, as the panel's confirmed.
    pub(super) fn stop_session(&mut self, ctx: &egui::Context, stop: Stop) {
        let Some(view) = &self.health else { return };
        let Some(sql) = view.server.stop(&stop.id, stop.whole) else {
            self.toasts
                .push(ctx, Toast::info("This database cannot stop that"));
            return;
        };
        let what = if stop.whole && view.server.cancels_query() {
            format!("Ended session {}", stop.id)
        } else {
            format!("Stopped {}", stop.id)
        };
        let request = self
            .backend
            .execute(view.connection, sql, ExecOptions::WRITE);
        self.health_requests.insert(request, Asked::Stop(what));
        if let Some(view) = &mut self.health {
            view.confirm = None;
        }
    }

    /// Reads the panel again every few seconds when it asks to.
    pub(super) fn health_tick(&mut self, ctx: &egui::Context) {
        let Some(view) = &self.health else { return };
        if !view.auto {
            return;
        }
        ctx.request_repaint_after(Duration::from_millis(500));
        if !view.busy() && view.last.is_none_or(|at| at.elapsed() >= EVERY) {
            self.refresh_health();
        }
    }
}

/// Draws the panel. Returns `false` once it should close.
pub fn show(ctx: &egui::Context, view: &mut Health, actions: &mut Vec<Action>) -> bool {
    let palette = Palette::current(ctx);
    let mut open = true;
    let modal = egui::Modal::new(egui::Id::new("server-health"))
        .frame(
            egui::Frame::new()
                .fill(palette.surface)
                .stroke(Stroke::new(1.0, palette.outline))
                .corner_radius(CornerRadius::same(14))
                .shadow(ctx.global_style().visuals.window_shadow)
                .inner_margin(egui::Margin::same(22)),
        )
        .show(ctx, |ui| {
            ui.set_width(920.0);
            ui.spacing_mut().item_spacing.y = 10.0;
            header(ui, view, &mut open, actions, &palette);
            sections(ui, view, actions, &palette);
            cards(ui, view, &palette);
            body(ui, view, actions, &palette);
            missing(ui, view, &palette);
        });
    if modal.should_close() {
        open = false;
    }
    open
}

fn header(
    ui: &mut egui::Ui,
    view: &mut Health,
    open: &mut bool,
    actions: &mut Vec<Action>,
    palette: &Palette,
) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(36.0), Sense::hover());
        ui.painter()
            .rect_filled(rect, CornerRadius::same(10), palette.accent_soft);
        widgets::paint_icon(ui, Icon::Activity, rect.shrink(9.0), palette.accent);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.label(
                RichText::new("Server health")
                    .font(theme::semibold(17.0))
                    .color(palette.text),
            );
            ui.label(
                RichText::new(format!("{} · {}", view.name, view.server.name()))
                    .color(palette.secondary),
            );
        });
        ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
            if widgets::icon_button(ui, Icon::Close, "Close").clicked() {
                *open = false;
            }
            if widgets::icon_button(ui, Icon::Refresh, "Read again").clicked() {
                actions.push(Action::HealthRefresh);
            }
            ui.checkbox(&mut view.auto, "Every 5 s")
                .on_hover_text("Read the overview and this section again every few seconds");
            if view.busy() {
                ui.add(egui::Spinner::new().size(14.0));
            }
        });
    });
}

fn sections(ui: &mut egui::Ui, view: &mut Health, actions: &mut Vec<Action>, palette: &Palette) {
    if view.sections.is_empty() {
        return;
    }
    let mut chosen = view.current;
    let labels: Vec<(usize, &str)> = view
        .sections
        .iter()
        .enumerate()
        .map(|(index, section)| (index, section.title))
        .collect();
    egui::ScrollArea::horizontal()
        .id_salt("health-sections")
        .show(ui, |ui| {
            widgets::segmented(ui, &mut chosen, &labels);
        });
    if chosen != view.current {
        view.current = chosen;
        view.selected = None;
        view.confirm = None;
        actions.push(Action::HealthSection(chosen));
    }
    let _ = palette;
}

fn cards(ui: &mut egui::Ui, view: &Health, palette: &Palette) {
    if view.current != 0 {
        return;
    }
    let Some(Loaded::Ready { sets, .. }) = view.loaded.get("overview") else {
        return;
    };
    let cards = health::overview(view.server, sets);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::splat(8.0);
        for card in &cards {
            let color = match card.level {
                Level::Info => palette.text,
                Level::Good => palette.success,
                Level::Warn => palette.warning,
                Level::Bad => palette.danger,
            };
            let response = egui::Frame::new()
                .fill(palette.window)
                .stroke(Stroke::new(1.0, palette.outline))
                .corner_radius(CornerRadius::same(8))
                .inner_margin(egui::Margin::symmetric(12, 8))
                .show(ui, |ui| {
                    ui.set_min_width(130.0);
                    ui.label(RichText::new(card.label).color(palette.dim).size(11.5));
                    ui.label(
                        RichText::new(&card.value)
                            .font(theme::semibold(15.0))
                            .color(color),
                    );
                })
                .response;
            response.on_hover_text(card.hint);
        }
    });
}

fn body(ui: &mut egui::Ui, view: &mut Health, actions: &mut Vec<Action>, palette: &Palette) {
    let Some(section) = view.section().cloned() else {
        ui.label(RichText::new("Nothing to show for this database.").color(palette.dim));
        return;
    };
    match view.loaded.get(section.id) {
        None | Some(Loaded::Loading) => {
            ui.add_space(20.0);
            ui.vertical_centered(|ui| ui.add(egui::Spinner::new()));
        }
        Some(Loaded::Failed(message)) => {
            egui::Frame::new()
                .fill(palette.danger.gamma_multiply(0.12))
                .corner_radius(CornerRadius::same(8))
                .inner_margin(egui::Margin::same(12))
                .show(ui, |ui| {
                    ui.label(RichText::new(message).color(palette.danger));
                });
        }
        Some(Loaded::Ready { sets, took }) => {
            if section.id == "overview" {
                ui.label(
                    RichText::new(format!("Read in {} ms", took.as_millis())).color(palette.dim),
                );
                return;
            }
            let table = section_table(view.server, section.id, sets);
            let stop_column = section.stoppable.and_then(|name| table.column(name));
            grid(ui, &table, &mut view.selected, palette);
            if let (Some(column), Some(row)) = (stop_column, view.selected)
                && let Some(id) = table.rows.get(row).and_then(|cells| cells.get(column))
            {
                stop_bar(ui, view, id, actions, palette);
            }
        }
    }
}

/// How `section`'s answer reads as a table.
fn section_table(server: Server, section: &str, sets: &[ResultSet]) -> Table {
    match (server, section) {
        (Server::Redis, "commands") => health::redis_commands(sets),
        (Server::Redis, "slowlog") => health::redis_slowlog(sets),
        (Server::Redis, "clients") => health::redis_clients(sets),
        (Server::Redis, "latency") => health::redis_latency(sets),
        (Server::Mongo, "operations") => health::mongo_operations(sets),
        (Server::Mongo, "profiler") => health::mongo_profile(sets),
        _ => sets.first().map(health::table).unwrap_or_default(),
    }
}

fn grid(ui: &mut egui::Ui, table: &Table, selected: &mut Option<usize>, palette: &Palette) {
    if table.rows.is_empty() {
        ui.add_space(16.0);
        ui.label(RichText::new("Nothing to show: the server reports no rows.").color(palette.dim));
        return;
    }
    let mut builder = TableBuilder::new(ui)
        .id_salt("health-grid")
        .striped(true)
        .resizable(true)
        .sense(Sense::click())
        .cell_layout(Layout::left_to_right(Align::Center))
        .max_scroll_height(340.0);
    for _ in &table.columns {
        builder = builder.column(Column::initial(110.0).at_least(40.0).clip(true));
    }
    builder
        .header(26.0, |mut header| {
            for name in &table.columns {
                header.col(|ui| {
                    ui.label(
                        RichText::new(name)
                            .font(theme::semibold(12.0))
                            .color(palette.text),
                    );
                });
            }
        })
        .body(|body| {
            body.rows(24.0, table.rows.len(), |mut row| {
                let index = row.index();
                row.set_selected(*selected == Some(index));
                for cell in &table.rows[index] {
                    row.col(|ui| {
                        ui.label(RichText::new(cell).font(theme::mono(11.5)));
                    });
                }
                if row.response().clicked() {
                    *selected = Some(index);
                }
            });
        });
}

/// What can be done to the selected session.
fn stop_bar(
    ui: &mut egui::Ui,
    view: &mut Health,
    id: &str,
    actions: &mut Vec<Action>,
    palette: &Palette,
) {
    ui.horizontal(|ui| {
        if let Some(stop) = view.confirm.clone() {
            let what = if stop.whole {
                "End the session of"
            } else {
                "Cancel the query of"
            };
            ui.label(RichText::new(format!("{what} {}?", stop.id)).color(palette.warning));
            if widgets::button(ui, Tone::Danger, None, "Yes").clicked() {
                actions.push(Action::HealthStop(stop));
            }
            if widgets::button(ui, Tone::Secondary, None, "Keep").clicked() {
                view.confirm = None;
            }
            return;
        }
        let cancel = view.server.stop(id, false).is_some();
        let whole = view.server.stop(id, true).is_some();
        if cancel
            && view.server.cancels_query()
            && view.server != Server::Mongo
            && widgets::button(ui, Tone::Secondary, None, "Cancel query").clicked()
        {
            view.confirm = Some(Stop {
                id: id.to_owned(),
                whole: false,
            });
        }
        if whole {
            let label = if view.server == Server::Mongo {
                "Kill operation"
            } else {
                "Terminate session"
            };
            if widgets::button(ui, Tone::Danger, None, label).clicked() {
                view.confirm = Some(Stop {
                    id: id.to_owned(),
                    whole: view.server != Server::Mongo,
                });
            }
        }
    });
}

/// What this database cannot show, and why.
fn missing(ui: &mut egui::Ui, view: &Health, palette: &Palette) {
    let items = view.server.missing();
    if items.is_empty() {
        return;
    }
    ui.add_space(4.0);
    for (what, why) in items {
        ui.label(
            RichText::new(format!("{what}: {why}"))
                .color(palette.dim)
                .size(11.5),
        );
    }
}
