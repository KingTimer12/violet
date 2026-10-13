//! The egui interface. [`VioletApp`] owns the interface state, draws it,
//! turns what the user does into [`action::Action`]s, and applies them
//! against the application layer.

mod action;
mod chrome;
mod connection_form;
#[cfg(debug_assertions)]
mod devtools;
mod editor;
pub mod icons;
mod results;
mod shortcuts;
mod sidebar;
mod state;
pub mod theme;
mod toasts;
mod widgets;
mod workspace;

use std::time::Instant;

use egui::{Frame, Id, Panel, Stroke};
use violet_app::{Backend, Event, ProfileService};
use violet_core::{ConnectionId, ErrorKind};
use violet_storage::JsonFile;

use crate::settings::Settings;
use crate::sql;
use action::{Action, RunScope};
use connection_form::{ConnectionForm, PasswordPrompt, TestState};
use state::{Explorer, Link, Loadable, NodeKey};
use theme::Palette;
use toasts::{Toast, Toasts};
use workspace::{Executed, Outcome, Running, TabId, Workspace};

/// Everything the interface needs from the rest of the app.
pub struct Services {
    pub backend: Backend,
    pub profiles: ProfileService,
    pub settings: JsonFile<Settings>,
}

pub struct VioletApp {
    backend: Backend,
    profiles: ProfileService,
    settings_file: JsonFile<Settings>,
    settings: Settings,
    explorer: Explorer,
    workspace: Workspace,
    toasts: Toasts,
    form: Option<ConnectionForm>,
    prompt: Option<PasswordPrompt>,
    confirm_delete: Option<ConnectionId>,
    /// Runs waiting for their tab's connection to open.
    queued: Vec<(TabId, RunScope)>,
    actions: Vec<Action>,
    #[cfg(debug_assertions)]
    snapshot: Option<devtools::Snapshot>,
}

impl VioletApp {
    pub fn new(
        ctx: &egui::Context,
        services: Services,
        settings: Settings,
        startup_errors: Vec<Toast>,
    ) -> Self {
        let mut app = Self {
            backend: services.backend,
            profiles: services.profiles,
            settings_file: services.settings,
            settings,
            explorer: Explorer::default(),
            workspace: Workspace::default(),
            toasts: Toasts::default(),
            form: None,
            prompt: None,
            confirm_delete: None,
            queued: Vec::new(),
            actions: Vec::new(),
            #[cfg(debug_assertions)]
            snapshot: devtools::Snapshot::from_env(),
        };
        for toast in startup_errors {
            app.toasts.push(ctx, toast);
        }
        app
    }

    fn save_settings(&mut self, ctx: &egui::Context) {
        if let Err(error) = self.settings_file.write(&self.settings) {
            self.toasts.push(
                ctx,
                Toast::error("Could not save settings", error.to_string()),
            );
        }
    }

    fn profile_name(&self, id: ConnectionId) -> String {
        self.profiles.get(id).map_or_else(
            || "connection".to_owned(),
            violet_core::ConnectionProfile::display_name,
        )
    }

    // ---- events from the backend ---------------------------------------

    fn handle_events(&mut self, ctx: &egui::Context) {
        for event in self.backend.drain() {
            self.handle_event(ctx, event);
        }
    }

    fn handle_event(&mut self, ctx: &egui::Context, event: Event) {
        match event {
            Event::Connected { id, server } => {
                self.toasts.push(
                    ctx,
                    Toast::success(format!("Connected to {}", self.profile_name(id))),
                );
                self.explorer.links.insert(id, Link::Online(server));
                self.explorer.forget(id);
                self.explorer.schemas.insert(id, Loadable::Loading);
                self.backend.load_schemas(id);
                let ready: Vec<_> = self
                    .queued
                    .iter()
                    .filter(|(tab, _)| {
                        self.workspace
                            .tabs
                            .iter()
                            .any(|t| t.id == *tab && t.connection == Some(id))
                    })
                    .copied()
                    .collect();
                self.queued.retain(|entry| !ready.contains(entry));
                for (tab, scope) in ready {
                    self.run(ctx, tab, scope);
                }
            }
            Event::ConnectFailed { id, error } => {
                self.queued.retain(|(tab, _)| {
                    !self
                        .workspace
                        .tabs
                        .iter()
                        .any(|t| t.id == *tab && t.connection == Some(id))
                });
                let needs_password = error.kind == ErrorKind::Authentication
                    && self
                        .profiles
                        .get(id)
                        .is_some_and(|profile| !profile.save_password);
                self.explorer
                    .links
                    .insert(id, Link::Failed(error.to_string()));
                if needs_password {
                    self.prompt = Some(PasswordPrompt {
                        id,
                        name: self.profile_name(id),
                        password: String::new(),
                    });
                } else {
                    self.toasts.push(
                        ctx,
                        Toast::error(
                            format!("Could not connect to {}", self.profile_name(id)),
                            error.message,
                        ),
                    );
                }
            }
            Event::Disconnected { id } => {
                self.explorer.links.insert(id, Link::Offline);
                self.explorer.forget(id);
            }
            Event::Schemas { id, result } => {
                if let Ok(schemas) = &result {
                    let any_open = self
                        .explorer
                        .expanded
                        .iter()
                        .any(|key| matches!(key, NodeKey::Schema(owner, _) if *owner == id));
                    if !any_open && schemas.iter().any(|schema| schema == "public") {
                        self.explorer
                            .expanded
                            .insert(NodeKey::Schema(id, "public".to_owned()));
                    }
                    // Reload what was open before.
                    for schema in schemas {
                        if self
                            .explorer
                            .is_expanded(&NodeKey::Schema(id, schema.clone()))
                        {
                            self.load_objects(id, schema.clone());
                        }
                    }
                }
                self.explorer
                    .schemas
                    .insert(id, Loadable::from_result(result));
            }
            Event::Objects { id, schema, result } => {
                if let Ok(objects) = &result {
                    for object in objects {
                        let key = NodeKey::Object(id, schema.clone(), object.name.clone());
                        if self.explorer.is_expanded(&key) {
                            self.load_columns(id, schema.clone(), object.name.clone());
                        }
                    }
                }
                self.explorer
                    .objects
                    .insert((id, schema), Loadable::from_result(result));
            }
            Event::Columns {
                id,
                schema,
                relation,
                result,
            } => {
                self.explorer
                    .columns
                    .insert((id, schema, relation), Loadable::from_result(result));
            }
            Event::QueryFinished { request, result } => {
                let Some(tab) = self.workspace.by_request(request) else {
                    return;
                };
                let Some(running) = tab.running.take() else {
                    return;
                };
                let connection = tab.connection;
                tab.active_set = 0;
                tab.selected_cell = None;
                tab.outcome = Some(match result {
                    Ok(output) => {
                        // Show the last set with rows: usually what was asked for.
                        tab.active_set = output
                            .sets
                            .iter()
                            .rposition(violet_core::ResultSet::has_rows)
                            .unwrap_or(0);
                        Outcome::Done { output }
                    }
                    Err(error) => {
                        if matches!(error.kind, ErrorKind::Connection | ErrorKind::NotConnected)
                            && let Some(id) = connection
                            && !self.backend.is_connected(id)
                        {
                            self.explorer.links.insert(id, Link::Offline);
                        }
                        Outcome::Failed {
                            error,
                            executed: running.executed,
                        }
                    }
                });
            }
            Event::TestFinished { request, result } => {
                if let Some(form) = &mut self.form
                    && matches!(form.test, TestState::Running(r) if r == request)
                {
                    form.test = match result {
                        Ok(server) => TestState::Passed(server),
                        Err(error) => TestState::Failed(error.to_string()),
                    };
                }
            }
        }
    }

    fn load_objects(&mut self, id: ConnectionId, schema: String) {
        self.explorer
            .objects
            .insert((id, schema.clone()), Loadable::Loading);
        self.backend.load_objects(id, schema);
    }

    fn load_columns(&mut self, id: ConnectionId, schema: String, relation: String) {
        self.explorer
            .columns
            .insert((id, schema.clone(), relation.clone()), Loadable::Loading);
        self.backend.load_columns(id, schema, relation);
    }

    fn connect(&mut self, id: ConnectionId, password: Option<violet_core::Secret>) {
        let Some(profile) = self.profiles.get(id).cloned() else {
            return;
        };
        if matches!(self.explorer.link(id), Link::Connecting) {
            return;
        }
        self.explorer.links.insert(id, Link::Connecting);
        self.backend.connect(profile, password);
    }

    // ---- running queries ------------------------------------------------

    fn run(&mut self, ctx: &egui::Context, tab_id: TabId, scope: RunScope) {
        let Some(tab) = self.workspace.get_mut(tab_id) else {
            return;
        };
        if tab.running.is_some() {
            return;
        }
        let range = match scope {
            RunScope::Script => 0..tab.sql.len(),
            RunScope::Statement if !tab.selection.is_empty() => tab.selection.clone(),
            RunScope::Statement => match sql::statement_at(&tab.sql, tab.selection.start) {
                Some(range) => range,
                None => 0..0,
            },
        };
        let text = tab
            .sql
            .get(range.clone())
            .unwrap_or_default()
            .trim()
            .to_owned();
        if text.is_empty() {
            self.toasts.push(ctx, Toast::info("Nothing to run"));
            return;
        }
        let Some(connection) = tab.connection else {
            self.toasts.push(
                ctx,
                Toast::info("Choose a connection for this editor first"),
            );
            return;
        };
        if !self.backend.is_connected(connection) {
            self.queued.push((tab_id, scope));
            self.connect(connection, None);
            return;
        }
        let offset = range.start
            + (tab.sql[range.clone()].len() - tab.sql[range.clone()].trim_start().len());
        let request = self.backend.execute(connection, text.clone());
        tab.running = Some(Running {
            request,
            started: Instant::now(),
            executed: Executed { offset, sql: text },
            cancelling: false,
        });
    }

    // ---- actions ----------------------------------------------------------

    fn apply_actions(&mut self, ctx: &egui::Context) {
        let actions = std::mem::take(&mut self.actions);
        for action in actions {
            self.apply(ctx, action);
        }
    }

    #[allow(clippy::too_many_lines)]
    fn apply(&mut self, ctx: &egui::Context, action: Action) {
        match action {
            Action::NewConnection => {
                if let Some(driver) = self.backend.registry().infos().first() {
                    self.form = Some(ConnectionForm::create(driver));
                }
            }
            Action::EditConnection(id) => {
                if let Some(profile) = self.profiles.get(id).cloned() {
                    let saved = profile.save_password
                        && self.profiles.secrets().get(id).ok().flatten().is_some();
                    self.form = Some(ConnectionForm::edit(profile, saved));
                }
            }
            Action::DuplicateConnection(id) => {
                if let Some(mut profile) = self.profiles.get(id).cloned() {
                    profile.id = ConnectionId::new();
                    profile.name = format!("{} copy", profile.display_name());
                    self.form = Some(ConnectionForm::edit(profile, false));
                    if let Some(form) = &mut self.form {
                        form.editing = false;
                    }
                }
            }
            Action::AskDeleteConnection(id) => self.confirm_delete = Some(id),
            Action::DeleteConnection(id) => {
                if self.backend.is_connected(id) {
                    self.backend.disconnect(id);
                }
                match self.profiles.remove(id) {
                    Ok(()) => {
                        self.explorer.remove(id);
                        for tab in &mut self.workspace.tabs {
                            if tab.connection == Some(id) {
                                tab.connection = None;
                            }
                        }
                    }
                    Err(error) => self.toasts.push(
                        ctx,
                        Toast::error("Could not delete the connection", error.to_string()),
                    ),
                }
            }
            Action::Connect(id) => self.connect(id, None),
            Action::ConnectWithPassword(id, password) => self.connect(id, Some(password)),
            Action::Disconnect(id) => {
                self.explorer.links.insert(id, Link::Offline);
                self.backend.disconnect(id);
            }
            Action::Refresh(id) => {
                self.explorer.forget(id);
                self.explorer.schemas.insert(id, Loadable::Loading);
                self.backend.load_schemas(id);
            }
            Action::LoadObjects(id, schema) => self.load_objects(id, schema),
            Action::LoadColumns(id, schema, relation) => self.load_columns(id, schema, relation),
            Action::SaveProfile {
                profile,
                password,
                connect,
            } => {
                let id = profile.id;
                let reconnect = self.backend.is_connected(id);
                match self.profiles.upsert(profile, password.as_ref()) {
                    Ok(()) => {
                        if connect || reconnect {
                            self.explorer.expanded.insert(NodeKey::Connection(id));
                            self.explorer.links.remove(&id);
                            self.connect(id, password);
                        }
                    }
                    Err(error) => self.toasts.push(
                        ctx,
                        Toast::error("Could not save the connection", error.to_string()),
                    ),
                }
            }
            Action::TestProfile { profile, password } => {
                // A saved profile tested without typing a password uses the
                // keychain's.
                let request = self.backend.test(profile, password);
                if let Some(form) = &mut self.form {
                    form.test = TestState::Running(request);
                }
            }
            Action::OpenEditor { connection, sql } => {
                let title = self.workspace.next_script_title();
                let connection =
                    connection.or_else(|| self.workspace.active().and_then(|tab| tab.connection));
                self.workspace.open(title, connection, sql, false);
            }
            Action::OpenData {
                connection,
                schema,
                relation,
            } => {
                let existing = self.workspace.tabs.iter().find(|tab| {
                    tab.data_view && tab.connection == Some(connection) && tab.title == relation
                });
                if let Some(tab) = existing {
                    self.workspace.active = Some(tab.id);
                    return;
                }
                let Some(sql) = self.backend.select_rows_sql(connection, &schema, &relation) else {
                    return;
                };
                let tab = self
                    .workspace
                    .open(relation, Some(connection), format!("{sql};"), true);
                self.run(ctx, tab, RunScope::Script);
            }
            Action::Run(tab, scope) => self.run(ctx, tab, scope),
            Action::Cancel(tab_id) => {
                if let Some(tab) = self.workspace.get_mut(tab_id)
                    && let Some(running) = &mut tab.running
                {
                    running.cancelling = true;
                    self.backend.cancel(running.request);
                }
                self.queued.retain(|(tab, _)| *tab != tab_id);
            }
            Action::CloseTab(id) => {
                if let Some(tab) = self.workspace.close(id)
                    && let Some(running) = tab.running
                {
                    self.backend.cancel(running.request);
                }
            }
            Action::SelectTab(id) => self.workspace.active = Some(id),
            Action::SetTabConnection(id, connection) => {
                if let Some(tab) = self.workspace.get_mut(id) {
                    tab.connection = connection;
                }
                if let Some(connection) = connection
                    && matches!(
                        self.explorer.link(connection),
                        Link::Offline | Link::Failed(_)
                    )
                {
                    self.connect(connection, None);
                }
            }
            Action::SetTheme(preference) => {
                ctx.set_theme(preference);
                self.settings.theme = preference.into();
                self.save_settings(ctx);
            }
            Action::ToggleSidebar => {
                self.settings.sidebar = !self.settings.sidebar;
                self.save_settings(ctx);
            }
            Action::Copy(text) => {
                ctx.copy_text(text);
                self.toasts.push(ctx, Toast::success("Copied to clipboard"));
            }
        }
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        let modal_open =
            self.form.is_some() || self.prompt.is_some() || self.confirm_delete.is_some();
        if modal_open {
            return;
        }
        let active = self.workspace.active;
        ctx.input_mut(|input| {
            // The script shortcut first: it is the statement one plus Shift.
            if input.consume_shortcut(&shortcuts::RUN_SCRIPT)
                && let Some(tab) = active
            {
                self.actions.push(Action::Run(tab, RunScope::Script));
            }
            if input.consume_shortcut(&shortcuts::RUN_STATEMENT)
                && let Some(tab) = active
            {
                self.actions.push(Action::Run(tab, RunScope::Statement));
            }
            if input.consume_shortcut(&shortcuts::CANCEL)
                && let Some(tab) = active
            {
                self.actions.push(Action::Cancel(tab));
            }
            if input.consume_shortcut(&shortcuts::NEW_EDITOR) {
                self.actions.push(Action::OpenEditor {
                    connection: None,
                    sql: String::new(),
                });
            }
            if input.consume_shortcut(&shortcuts::CLOSE_TAB)
                && let Some(tab) = active
            {
                self.actions.push(Action::CloseTab(tab));
            }
            if input.consume_shortcut(&shortcuts::NEW_CONNECTION) {
                self.actions.push(Action::NewConnection);
            }
            if input.consume_shortcut(&shortcuts::TOGGLE_SIDEBAR) {
                self.actions.push(Action::ToggleSidebar);
            }
        });
    }

    fn central(&mut self, ui: &mut egui::Ui) {
        let palette = Palette::current(ui.ctx());
        if self.workspace.tabs.is_empty() {
            chrome::welcome(ui, self.profiles.all(), &self.explorer, &mut self.actions);
            return;
        }
        Panel::top("tab-strip")
            .exact_size(36.0)
            .frame(Frame::NONE)
            .show_separator_line(false)
            .show(ui, |ui| {
                chrome::tab_strip(ui, &self.workspace, self.profiles.all(), &mut self.actions)
            });

        let Some(active) = self.workspace.active else {
            return;
        };
        let profiles = self.profiles.all();
        let explorer = &self.explorer;
        let actions = &mut self.actions;
        let Some(tab) = self.workspace.tabs.iter_mut().find(|tab| tab.id == active) else {
            return;
        };

        Panel::top(Id::new(("toolbar", tab.id)))
            .exact_size(44.0)
            .frame(Frame::NONE.fill(palette.window))
            .show_separator_line(false)
            .show(ui, |ui| {
                chrome::toolbar(ui, tab, profiles, explorer, actions)
            });

        let total = ui.available_height();
        let results_height = if tab.data_view {
            total * 0.78
        } else {
            total * 0.5
        };
        Panel::bottom(Id::new(("results", tab.id)))
            .resizable(true)
            .default_size(results_height)
            .size_range(90.0..=(total - 80.0).max(120.0))
            .frame(
                Frame::NONE
                    .fill(palette.window)
                    .stroke(Stroke::new(1.0, palette.outline)),
            )
            .show(ui, |ui| results::show(ui, tab, actions));

        egui::CentralPanel::default()
            .frame(Frame::NONE.fill(palette.window))
            .show(ui, |ui| editor::show(ui, tab));
    }

    fn modals(&mut self, ctx: &egui::Context) {
        if let Some(form) = &mut self.form {
            let drivers = self.backend.registry().infos();
            if !connection_form::show(ctx, form, &drivers, &mut self.actions) {
                self.form = None;
            }
        }
        if let Some(prompt) = &mut self.prompt
            && !connection_form::show_prompt(ctx, prompt, &mut self.actions)
        {
            self.prompt = None;
        }
        if let Some(id) = self.confirm_delete {
            let name = self.profile_name(id);
            match connection_form::confirm_delete(ctx, &name) {
                Some(true) => {
                    self.actions.push(Action::DeleteConnection(id));
                    self.confirm_delete = None;
                }
                Some(false) => self.confirm_delete = None,
                None => {}
            }
        }
    }
}

impl eframe::App for VioletApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_events(ctx);
        self.shortcuts(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let palette = Palette::current(&ctx);
        fastframe_macos::align_traffic_lights(frame, &ctx, chrome::TITLE_BAR_HEIGHT);

        let title = self
            .workspace
            .active()
            .map(|tab| match tab.connection {
                Some(id) => format!("{} · {}", self.profile_name(id), tab.title),
                None => tab.title.clone(),
            })
            .unwrap_or_default();
        Panel::top("title-bar")
            .exact_size(chrome::TITLE_BAR_HEIGHT)
            .frame(
                Frame::NONE
                    .fill(palette.panel)
                    .stroke(Stroke::new(1.0, palette.outline)),
            )
            .show(ui, |ui| {
                chrome::title_bar(ui, &title, self.settings.theme.into(), &mut self.actions)
            });

        Panel::bottom("status-bar")
            .exact_size(26.0)
            .frame(
                Frame::NONE
                    .fill(palette.panel)
                    .stroke(Stroke::new(1.0, palette.outline)),
            )
            .show(ui, |ui| {
                chrome::status_bar(
                    ui,
                    self.workspace.active(),
                    self.profiles.all(),
                    &self.explorer,
                )
            });

        let mut sidebar_open = self.settings.sidebar;
        Panel::left("sidebar")
            .resizable(true)
            .default_size(280.0)
            .size_range(200.0..=520.0)
            .frame(
                Frame::NONE
                    .fill(palette.panel)
                    .stroke(Stroke::new(1.0, palette.outline)),
            )
            .show_collapsible(ui, &mut sidebar_open, |ui| {
                sidebar::show(
                    ui,
                    self.profiles.all(),
                    &mut self.explorer,
                    &mut self.actions,
                );
            });
        if sidebar_open != self.settings.sidebar {
            self.actions.push(Action::ToggleSidebar);
        }

        egui::CentralPanel::default()
            .frame(Frame::NONE.fill(palette.window))
            .show(ui, |ui| self.central(ui));

        self.modals(&ctx);
        self.toasts.show(&ctx);
        #[cfg(debug_assertions)]
        self.drive_snapshot(&ctx);
        self.apply_actions(&ctx);
    }
}

/// An error notice, for start-up problems found before the app exists.
pub fn toast_error(title: impl Into<String>, body: impl Into<String>) -> Toast {
    Toast::error(title, body)
}
