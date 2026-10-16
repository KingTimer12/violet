//! The egui interface. [`VioletApp`] owns the interface state, draws it,
//! turns what the user does into [`action::Action`]s, and applies them
//! against the application layer.

mod action;
mod chrome;
mod connection_form;
mod copy_flow;
#[cfg(debug_assertions)]
mod devtools;
mod edit_flow;
mod editor;
mod export_flow;
mod fonts;
mod history;
pub mod icons;
mod providers;
mod reference;
mod results;
mod script_flow;
mod selection;
mod shortcuts;
mod sidebar;
mod state;
mod task_flow;
mod tasks;
pub mod theme;
mod toasts;
mod widgets;
mod workspace;

use std::borrow::Cow;
use std::collections::HashMap;
use std::hash::{Hash as _, Hasher as _};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use egui::{Frame, Id, Panel, Stroke};
use violet_app::{Backend, CachePolicy, Event, ExecOptions, ProfileService, TaskId};
use violet_core::ports::{HistoryEntry, QueryHistory};
use violet_core::{ConnectionId, ConnectionProfile, DriverId, ErrorKind, Page};
use violet_storage::{JsonFile, SavedScript, ScriptStore};

use crate::completion::{self, Need};
use crate::settings::Settings;
use crate::sql;
use action::{Action, RunScope};
use connection_form::{ConnectionForm, PasswordPrompt, TestState};
use state::{ConnectionCatalog, Explorer, Link, Loadable, NodeKey};
use theme::Palette;
use toasts::{Toast, Toasts};
use workspace::{
    Count, DataView, Executed, Outcome, Paging, Running, SavedSession, TabId, TabKind, Workspace,
};

/// The most queries the history keeps.
pub const HISTORY_SIZE: usize = 5_000;

/// The panels' sizes as drawn, to save the ones the user drags.
#[derive(Debug, Default)]
struct Layout {
    /// The results' share of an editor tab's height.
    results: Option<f32>,
    sidebar: Option<f32>,
    /// A panel changed size while the pointer was down.
    dragged: bool,
}

/// Everything the interface needs from the rest of the app.
pub struct Services {
    pub backend: Backend,
    pub profiles: ProfileService,
    pub settings: JsonFile<Settings>,
    /// Where providers are installed, to show in their dialog.
    pub providers_dir: PathBuf,
    pub history: Arc<dyn QueryHistory>,
    /// The editors open last time.
    pub session: JsonFile<SavedSession>,
    pub scripts: ScriptStore,
    /// The update worker; `None` when updates are off.
    pub updates: Option<crate::update::Updates>,
    /// Set when this launch is a just-installed update, to confirm once
    /// the window is up.
    pub update_receipt: Option<fastframe_update::Receipt>,
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
    providers: providers::Providers,
    providers_dir: PathBuf,
    prompt: Option<PasswordPrompt>,
    scripts: ScriptStore,
    /// The scripts saved for connections, as last listed.
    script_list: Vec<SavedScript>,
    script_prompt: Option<script_flow::ScriptPrompt>,
    cell_editor: Option<edit_flow::CellEditor>,
    edit_review: Option<edit_flow::EditReview>,
    run_review: Option<edit_flow::RunReview>,
    /// Grid changes being written.
    applying: HashMap<violet_app::RequestId, edit_flow::Applying>,
    /// The row the selected foreign key cell points at.
    reference: Option<reference::Reference>,
    /// Exports waiting for every row to be read.
    exports: HashMap<violet_app::RequestId, export_flow::PendingExport>,
    copy_dialog: Option<copy_flow::CopyDialog>,
    /// The format and header choice advanced copy starts with.
    copy_choice: (crate::export::Format, bool),
    confirm_delete: Option<ConnectionId>,
    /// Runs waiting for their tab's connection to open.
    queued: Vec<(TabId, RunScope)>,
    actions: Vec<Action>,
    fonts: fonts::Fallbacks,
    /// Connection names not yet checked against the fonts.
    names_unchecked: bool,
    history_store: Arc<dyn QueryHistory>,
    /// Oldest first.
    history: Vec<HistoryEntry>,
    history_view: Option<history::HistoryView>,
    session_file: JsonFile<SavedSession>,
    /// A fingerprint of the editors as last saved, and when they were last
    /// compared.
    session_saved: u64,
    session_checked: Instant,
    /// Panel sizes measured this frame.
    layout: Layout,
    /// Each driver's backups and restores, once asked for.
    tasks: HashMap<DriverId, Loadable<violet_app::Offer>>,
    /// Client programs being downloaded, or why they could not be.
    tools: HashMap<DriverId, task_flow::Tools>,
    task_dialog: Option<tasks::TaskDialog>,
    /// Backups and restores still running.
    started: HashMap<TaskId, task_flow::Started>,
    updates: Option<crate::update::Updates>,
    update_receipt: Option<fastframe_update::Receipt>,
    #[cfg(debug_assertions)]
    snapshot: Option<devtools::Snapshot>,
}

fn connection_name(
    profiles: &ProfileService,
    explorer: &Explorer,
    id: ConnectionId,
) -> Option<String> {
    match explorer.opened.get(&id) {
        Some((parent, database)) => profiles
            .get(*parent)
            .map(|profile| format!("{} · {database}", profile.display_name())),
        None => profiles.get(id).map(ConnectionProfile::display_name),
    }
}

/// The profile a session opens with: a saved connection's, or for another
/// of its databases, the connection's on that database. Its id stays the
/// connection's, where the password is kept.
fn session_profile(
    profiles: &ProfileService,
    explorer: &Explorer,
    id: ConnectionId,
) -> Option<ConnectionProfile> {
    match explorer.opened.get(&id) {
        Some((parent, database)) => profiles.get(*parent).map(|profile| {
            let mut profile = profile.clone();
            profile.database.clone_from(database);
            profile
        }),
        None => profiles.get(id).cloned(),
    }
}

/// The saved connections, then the other databases opened, each as a
/// connection of its own: what editors can run on.
fn connections<'a>(
    profiles: &'a ProfileService,
    explorer: &Explorer,
) -> Cow<'a, [ConnectionProfile]> {
    if explorer.opened.is_empty() {
        return Cow::Borrowed(profiles.all());
    }
    let mut opened: Vec<_> = explorer.opened.keys().copied().collect();
    opened.sort_by_key(|id| connection_name(profiles, explorer, *id));
    let mut all = profiles.all().to_vec();
    all.extend(opened.into_iter().filter_map(|id| {
        let mut profile = session_profile(profiles, explorer, id)?;
        profile.name = connection_name(profiles, explorer, id)?;
        profile.id = id;
        Some(profile)
    }));
    Cow::Owned(all)
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
            providers: providers::Providers::default(),
            providers_dir: services.providers_dir,
            prompt: None,
            scripts: services.scripts,
            script_list: Vec::new(),
            script_prompt: None,
            cell_editor: None,
            edit_review: None,
            run_review: None,
            applying: HashMap::new(),
            reference: None,
            exports: HashMap::new(),
            copy_dialog: None,
            copy_choice: (crate::export::Format::Tsv, true),
            confirm_delete: None,
            queued: Vec::new(),
            actions: Vec::new(),
            fonts: fonts::Fallbacks::default(),
            names_unchecked: true,
            history_store: services.history,
            history: Vec::new(),
            history_view: None,
            session_file: services.session,
            session_saved: 0,
            session_checked: Instant::now(),
            layout: Layout::default(),
            tasks: HashMap::new(),
            tools: HashMap::new(),
            task_dialog: None,
            started: HashMap::new(),
            updates: services.updates,
            update_receipt: services.update_receipt,
            #[cfg(debug_assertions)]
            snapshot: devtools::Snapshot::from_env(),
        };
        for toast in startup_errors {
            app.toasts.push(ctx, toast);
        }
        match app.history_store.load() {
            Ok(entries) => app.history = entries,
            Err(error) => app.toasts.push(
                ctx,
                Toast::error("The query history could not be read", error.to_string()),
            ),
        }
        app.restore_session(ctx);
        app.reattach_scripts();
        app.reload_scripts(ctx);
        app.load_providers();
        app
    }

    /// Opens the editors that were open when Violet last closed. Screenshot
    /// scenes start from none.
    fn restore_session(&mut self, ctx: &egui::Context) {
        #[cfg(debug_assertions)]
        if self
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| !snapshot.restores())
        {
            return;
        }
        match self.session_file.read() {
            Ok(Some(session)) => {
                self.session_saved = fingerprint(&session);
                let profiles = &self.profiles;
                self.workspace
                    .restore(session, |id| profiles.get(id).is_some());
            }
            Ok(None) => {}
            Err(error) => self.toasts.push(
                ctx,
                Toast::error("The open editors could not be restored", error.to_string()),
            ),
        }
    }

    /// Writes the open editors when they changed, at most every couple of
    /// seconds, and always when `now`.
    fn save_session(&mut self, now: bool) {
        if !now && self.session_checked.elapsed() < Duration::from_secs(2) {
            return;
        }
        #[cfg(debug_assertions)]
        if self.snapshot.is_some() {
            return;
        }
        self.session_checked = Instant::now();
        let session = self.workspace.save();
        let print = fingerprint(&session);
        if print == self.session_saved {
            return;
        }
        match self.session_file.write(&session) {
            Ok(()) => self.session_saved = print,
            Err(error) => log::warn!("session: {error}"),
        }
    }

    /// Keeps a finished run in the history.
    fn record(&mut self, entry: HistoryEntry) {
        if let Err(error) = self.history_store.append(&entry) {
            log::warn!("history: {error}");
        }
        self.history.push(entry);
        if self.history.len() > HISTORY_SIZE {
            let excess = self.history.len() - HISTORY_SIZE;
            self.history.drain(..excess);
            if let Some(view) = &mut self.history_view {
                view.selected = None;
            }
        }
    }

    fn load_providers(&mut self) {
        if !matches!(self.providers.listing, providers::Listing::Loading) {
            self.providers.listing = providers::Listing::Loading;
            self.backend.load_providers();
        }
    }

    /// Opens the connection form on the first database on offer.
    fn new_connection(&mut self) -> bool {
        let choices = self.providers.choices(self.backend.registry().infos());
        let Some(choice) = choices.first() else {
            return false;
        };
        self.form = Some(ConnectionForm::create(&choice.info));
        true
    }

    /// Keeps the sizes the user dragged the sidebar and the results to, once
    /// the drag ends. Sizes that change as the window does are not saved.
    fn remember_layout(&mut self, ctx: &egui::Context) {
        let dragging = ctx.input(|input| input.pointer.any_down());
        let results = self
            .layout
            .results
            .filter(|share| (share - self.settings.results_share).abs() > 0.002);
        let sidebar = self
            .layout
            .sidebar
            .filter(|width| (width - self.settings.sidebar_width).abs() > 0.5);
        if results.is_none() && sidebar.is_none() {
            self.layout.dragged = false;
            return;
        }
        if dragging {
            self.layout.dragged = true;
            return;
        }
        if std::mem::take(&mut self.layout.dragged) {
            if let Some(share) = results {
                self.settings.results_share = share;
            }
            if let Some(width) = sidebar {
                self.settings.sidebar_width = width;
            }
            self.save_settings(ctx);
        }
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
        self.connection_name(id)
            .unwrap_or_else(|| "connection".to_owned())
    }

    fn connection_name(&self, id: ConnectionId) -> Option<String> {
        connection_name(&self.profiles, &self.explorer, id)
    }

    fn profile(&self, id: ConnectionId) -> Option<ConnectionProfile> {
        session_profile(&self.profiles, &self.explorer, id)
    }

    /// Whether `id` is a saved connection that lists its server's databases.
    /// The saved connection a session belongs to: itself, or the one it
    /// opened another database of.
    fn root(&self, id: ConnectionId) -> ConnectionId {
        self.explorer
            .opened
            .get(&id)
            .map_or(id, |(parent, _)| *parent)
    }

    /// The tree's selection moved: when it is on a database (or something
    /// inside one) of the server the active editor runs on, the editor
    /// switches to that database, as DBeaver's navigator links to its
    /// editor. An editor with no connection takes it too. Running editors
    /// and ones with unsaved grid changes stay where they are.
    fn follow_tree(&mut self, ctx: &egui::Context) {
        let session = match self.explorer.selected.clone() {
            Some(NodeKey::Database(parent, database)) => self.database_session(parent, &database),
            Some(
                NodeKey::Connection(id)
                | NodeKey::Schema(id, _)
                | NodeKey::Group(id, _, _)
                | NodeKey::Object(id, _, _),
            ) => Some(id),
            Some(NodeKey::Scripts(_)) | None => None,
        };
        let Some(session) = session else {
            return;
        };
        let root = self.root(session);
        let Some(tab) = self.workspace.active() else {
            return;
        };
        let follows = matches!(tab.kind, TabKind::Script)
            && tab.running.is_none()
            && tab.edits.is_empty()
            && tab.connection != Some(session)
            && tab
                .connection
                .is_none_or(|current| self.root(current) == root);
        if !follows {
            return;
        }
        let tab_id = tab.id;
        if let Some(name) = self.connection_name(session) {
            self.toasts
                .push(ctx, Toast::info(format!("The editor now runs on {name}")));
        }
        self.actions
            .push(Action::SetTabConnection(tab_id, Some(session)));
    }

    /// The SQL dialect of a connection's driver.
    fn dialect(&self, id: ConnectionId) -> violet_core::SqlDialect {
        connections(&self.profiles, &self.explorer)
            .iter()
            .find(|profile| profile.id == id)
            .and_then(|profile| self.backend.registry().get(&profile.driver))
            .map_or(violet_core::SqlDialect::ANSI, |driver| {
                driver.info().dialect
            })
    }

    fn shows_all_databases(&self, id: ConnectionId) -> bool {
        self.profiles.get(id).is_some_and(|profile| {
            profile.show_all_databases
                && self
                    .backend
                    .registry()
                    .get(&profile.driver)
                    .is_some_and(|driver| driver.info().capabilities.databases)
        })
    }

    /// Closes the sessions on other databases of `parent`.
    fn disconnect_opened(&mut self, parent: ConnectionId) {
        for id in self.explorer.opened_under(parent) {
            if self.backend.is_connected(id) {
                self.backend.disconnect(id);
            }
            self.explorer.links.insert(id, Link::Offline);
        }
    }

    // ---- events from the backend ---------------------------------------

    fn handle_events(&mut self, ctx: &egui::Context) {
        for event in self.backend.drain() {
            self.handle_event(ctx, event);
        }
    }

    fn handle_event(&mut self, ctx: &egui::Context, event: Event) {
        self.fonts.cover_event(ctx, &event);
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
                if self.shows_all_databases(id) {
                    self.explorer.databases.insert(id, Loadable::Loading);
                    self.backend.load_databases(id);
                }
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
                        .profile(id)
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
            Event::Databases { id, result } => {
                self.explorer
                    .databases
                    .insert(id, Loadable::from_result(result));
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
            Event::ForeignKeys { id, schema, result } => {
                self.explorer
                    .foreign_keys
                    .insert((id, schema), Loadable::from_result(result));
            }
            Event::QueryFinished { request, result } => {
                if let Some(applying) = self.applying.remove(&request) {
                    self.edits_applied(ctx, applying, &result);
                    return;
                }
                if let Some(pending) = self.exports.remove(&request) {
                    self.export_read(ctx, pending, result);
                    return;
                }
                if self.awaits_reference(request) {
                    self.reference_finished(result);
                    return;
                }
                let Some(tab) = self.workspace.by_request(request) else {
                    return;
                };
                let Some(running) = tab.running.take() else {
                    return;
                };
                let connection = tab.connection;
                // Runs from editors go to the history; pages and data tabs
                // do not.
                let entry = (!running.append && tab.data().is_none()).then(|| HistoryEntry {
                    at: SystemTime::now()
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .map_or(0, |since| {
                            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
                        }),
                    connection,
                    source: connection
                        .and_then(|id| connection_name(&self.profiles, &self.explorer, id))
                        .unwrap_or_default(),
                    sql: sql::redact_secrets(&running.executed.sql),
                    elapsed_ms: u64::try_from(
                        result
                            .as_ref()
                            .map_or_else(|_| running.started.elapsed(), |output| output.elapsed)
                            .as_millis(),
                    )
                    .unwrap_or(u64::MAX),
                    rows: result.as_ref().ok().map(|output| {
                        output
                            .sets
                            .iter()
                            .map(|set| {
                                if set.has_rows() {
                                    set.rows.len() as u64
                                } else {
                                    set.affected.unwrap_or(0)
                                }
                            })
                            .sum()
                    }),
                    // A server may quote the statement back, password
                    // included.
                    error: result.as_ref().err().map(|error| {
                        if sql::sets_secret(&running.executed.sql) {
                            "Failed (message hidden: the statement carries a password)".to_owned()
                        } else {
                            error.message.clone()
                        }
                    }),
                });
                if let Ok(output) = &result
                    && let Some(page) = output.page
                    && let Some(paging) = &mut tab.paging
                {
                    let fetched = output.sets.first().map_or(0, |set| set.rows.len());
                    paging.loaded = page.offset + fetched as u64;
                    paging.has_more = page.has_more;
                }
                // The next page joins the rows already shown.
                if running.append
                    && let Ok(output) = &result
                    && let Some(Outcome::Done { output: shown }) = &mut tab.outcome
                    && let (Some(set), Some(next)) = (shown.sets.first_mut(), output.sets.first())
                {
                    set.rows.append(&next.rows);
                    shown.page = output.page;
                    shown.elapsed = output.elapsed;
                    shown.cached = output.cached;
                    return;
                }
                if !running.append {
                    tab.active_set = 0;
                    tab.grid_selection.clear();
                }
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
                if let Some(entry) = entry {
                    self.record(entry);
                }
            }
            Event::Counted { request, result } => {
                if let Some(paging) = self
                    .workspace
                    .by_count(request)
                    .and_then(|tab| tab.paging.as_mut())
                {
                    paging.total = Some(match result {
                        Ok(count) => Count::Done(count),
                        Err(error) => Count::Failed(error.message),
                    });
                }
            }
            Event::Providers(result) => {
                match result {
                    Ok(listings) => self.providers.listing = providers::Listing::Ready(listings),
                    Err(error) => {
                        if self.providers.awaiting_new_connection {
                            self.toasts.push(
                                ctx,
                                Toast::error(
                                    "Could not load the database providers",
                                    error.message.clone(),
                                ),
                            );
                        }
                        self.providers.listing = providers::Listing::Failed(error.message);
                    }
                }
                if std::mem::take(&mut self.providers.awaiting_new_connection) {
                    self.new_connection();
                }
            }
            Event::ProviderInstalling { id } => {
                self.toasts.push(
                    ctx,
                    Toast::info(format!(
                        "Downloading the {} provider…",
                        self.providers.name(&id)
                    )),
                );
                self.providers.busy.insert(id);
            }
            Event::ProviderInstalled { id, result } => {
                self.providers.busy.remove(&id);
                match result {
                    Ok(info) => {
                        self.providers.errors.remove(&id);
                        self.toasts.push(
                            ctx,
                            Toast::success(format!("{} provider installed", info.name)),
                        );
                    }
                    // Connections report it too; the dialog keeps it.
                    Err(error) => {
                        self.providers.errors.insert(id, error.message);
                    }
                }
                self.load_providers();
            }
            Event::ProviderRemoved { id, result } => {
                self.providers.busy.remove(&id);
                match result {
                    Ok(()) => {
                        self.toasts.push(
                            ctx,
                            Toast::info(format!("{} provider removed", self.providers.name(&id))),
                        );
                        self.providers.errors.remove(&id);
                    }
                    Err(error) => {
                        self.providers.errors.insert(id, error.message);
                    }
                }
                self.load_providers();
            }
            event @ (Event::Tasks { .. }
            | Event::ToolsDownloading { .. }
            | Event::ToolsReady { .. }
            | Event::ProgramsFound { .. }
            | Event::TaskPreview { .. }
            | Event::InputInspected { .. }
            | Event::TaskOutput { .. }
            | Event::TaskFinished { .. }) => self.task_event(ctx, event),
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

    /// Loads what completion asked for, unless it is loaded or on its way.
    fn load_for_completion(&mut self, id: ConnectionId, needs: Vec<Need>) {
        if !self.explorer.link(id).is_online() {
            return;
        }
        for need in needs {
            match need {
                Need::Objects(schema) => {
                    if !self.explorer.objects.contains_key(&(id, schema.clone())) {
                        self.load_objects(id, schema);
                    }
                }
                Need::Columns(schema, relation) => {
                    let key = (id, schema.clone(), relation.clone());
                    if !self.explorer.columns.contains_key(&key) {
                        self.load_columns(id, schema, relation);
                    }
                }
                Need::ForeignKeys(schema) => {
                    let key = (id, schema.clone());
                    if let std::collections::hash_map::Entry::Vacant(entry) =
                        self.explorer.foreign_keys.entry(key)
                    {
                        entry.insert(Loadable::Loading);
                        self.backend.load_foreign_keys(id, schema);
                    }
                }
            }
        }
    }

    fn connect(&mut self, id: ConnectionId, password: Option<violet_core::Secret>) {
        let Some(profile) = self.profile(id) else {
            return;
        };
        if matches!(self.explorer.link(id), Link::Connecting) {
            return;
        }
        self.explorer.links.insert(id, Link::Connecting);
        self.backend.connect_to(id, profile, password);
    }

    /// Opens `database` of connection `parent`: its own database is the
    /// connection's session, any other gets a session of its own.
    fn open_database(&mut self, parent: ConnectionId, database: String) {
        let own = matches!(
            self.explorer.link(parent),
            Link::Online(server) if server.database == database
        );
        let key = NodeKey::Database(parent, database.clone());
        if own {
            self.explorer.expanded.insert(key);
            return;
        }
        let Some(id) = self.database_session(parent, &database) else {
            return;
        };
        self.explorer.expanded.insert(key);
        if !matches!(self.explorer.link(id), Link::Online(_)) {
            self.connect(id, None);
        }
    }

    // ---- running queries ------------------------------------------------

    #[hotpath::measure]
    fn run(&mut self, ctx: &egui::Context, tab_id: TabId, scope: RunScope) {
        self.run_checked(ctx, tab_id, scope, false);
    }

    /// Runs an editor; statements that change every row of a table run
    /// only once the user has `confirmed` them.
    fn run_checked(
        &mut self,
        ctx: &egui::Context,
        tab_id: TabId,
        scope: RunScope,
        confirmed: bool,
    ) {
        if self.keeps_edits(ctx, tab_id) {
            return;
        }
        let is_data = self
            .workspace
            .get(tab_id)
            .is_some_and(|tab| tab.data().is_some());
        if scope == RunScope::Data || is_data {
            self.run_data(ctx, tab_id, CachePolicy::Use);
            return;
        }
        let Some(tab) = self.workspace.get_mut(tab_id) else {
            return;
        };
        if tab.running.is_some() {
            return;
        }
        let explain = matches!(scope, RunScope::Explain | RunScope::ExplainAnalyze);
        let range = match scope {
            RunScope::Script | RunScope::Data => 0..tab.sql.len(),
            RunScope::Statement | RunScope::Explain | RunScope::ExplainAnalyze
                if !tab.selection.is_empty() =>
            {
                tab.selection.clone()
            }
            RunScope::Statement | RunScope::Explain | RunScope::ExplainAnalyze => {
                match sql::statement_at(&tab.sql, tab.selection.start) {
                    Some(range) => range,
                    None => 0..0,
                }
            }
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
        if explain {
            self.explain(ctx, tab_id, connection, offset, &text, scope);
            return;
        }
        if !confirmed {
            let writes = self.unbounded_writes(&text, connection);
            if !writes.is_empty() {
                self.run_review = Some(edit_flow::RunReview {
                    tab: tab_id,
                    scope,
                    writes,
                });
                return;
            }
        }
        // A new run starts unsorted.
        if let Some(tab) = self.workspace.get_mut(tab_id) {
            tab.sort = None;
            tab.unsorted = None;
        }
        // Reads may come from the cache, and a single query is read a page
        // at a time, as DBeaver does.
        let plan = sql::plan(&text);
        let cache = if plan.cacheable {
            CachePolicy::Use
        } else {
            CachePolicy::Off
        };
        let (text, page) = match plan.pageable {
            Some(single) => (single, Some(Page::first())),
            None => (text, None),
        };
        let options = ExecOptions { cache, page };
        let executed = Executed {
            offset,
            sql: text,
            shift: 0,
        };
        self.start(tab_id, connection, executed, options, false);
    }

    /// Reads a tab's rows again, sorted by the server on the column at
    /// `column`, or unsorted. A data tab sorts its relation; a query is
    /// wrapped and sorted by the column's position, which holds even when
    /// two columns share a name.
    fn sort(
        &mut self,
        ctx: &egui::Context,
        tab_id: TabId,
        column: usize,
        descending: Option<bool>,
    ) {
        if self.keeps_edits(ctx, tab_id) {
            return;
        }
        let Some(tab) = self.workspace.get_mut(tab_id) else {
            return;
        };
        if tab.running.is_some() {
            return;
        }
        let name = match &tab.outcome {
            Some(Outcome::Done { output }) => output
                .sets
                .first()
                .and_then(|set| set.columns.get(column))
                .map(|meta| meta.name.clone()),
            _ => None,
        };
        let Some(name) = name else { return };
        if let Some(view) = tab.data_mut() {
            view.order = descending.map(|descending| violet_core::OrderBy {
                column: name,
                descending,
            });
            self.run_data(ctx, tab_id, CachePolicy::Use);
            return;
        }
        let (Some(connection), Some(paging), Some((last, _))) =
            (tab.connection, &tab.paging, &tab.last_run)
        else {
            return;
        };
        let base = tab.unsorted.clone().unwrap_or_else(|| paging.base.clone());
        const PREFIX: &str = "SELECT * FROM (\n";
        let (sql, shift) = match descending {
            Some(descending) => (
                format!(
                    "{PREFIX}{base}\n) AS violet_sorted\nORDER BY {} {}",
                    column + 1,
                    if descending { "DESC" } else { "ASC" }
                ),
                PREFIX.chars().count(),
            ),
            None => (base.clone(), 0),
        };
        let executed = Executed {
            offset: last.offset,
            sql,
            shift,
        };
        let options = ExecOptions {
            cache: CachePolicy::Use,
            page: Some(Page::first()),
        };
        tab.sort = descending.map(|descending| (column, descending));
        tab.unsorted = descending.is_some().then_some(base);
        self.start(tab_id, connection, executed, options, false);
    }

    /// Whether `tab_id` has changed cells a new read would drop: the user
    /// saves or discards them first.
    fn keeps_edits(&mut self, ctx: &egui::Context, tab_id: TabId) -> bool {
        let pending = self
            .workspace
            .get(tab_id)
            .is_some_and(|tab| !tab.edits.is_empty());
        if pending {
            self.toasts.push(
                ctx,
                Toast::error(
                    "Changed cells are not saved",
                    "Save or discard them before reading the rows again.",
                ),
            );
        }
        pending
    }

    /// Runs one statement under `EXPLAIN`, or `EXPLAIN ANALYZE` when it
    /// only reads: analysing runs it for real.
    fn explain(
        &mut self,
        ctx: &egui::Context,
        tab_id: TabId,
        connection: ConnectionId,
        offset: usize,
        text: &str,
        scope: RunScope,
    ) {
        if sql::statements(text).len() != 1 {
            self.toasts.push(
                ctx,
                Toast::info("Explain one statement at a time: select it, or put the cursor in it"),
            );
            return;
        }
        let analyze = scope == RunScope::ExplainAnalyze;
        if analyze && !sql::plan(text).reads_only {
            self.toasts.push(
                ctx,
                Toast::error(
                    "EXPLAIN ANALYZE runs the statement",
                    "Violet only analyses statements that read. Use Explain to see the plan of one that writes.",
                ),
            );
            return;
        }
        let statement = text.trim_end().trim_end_matches(';').trim_end();
        let Some(sent) = self.backend.explain_sql(connection, statement, analyze) else {
            return;
        };
        let shift = sent
            .find(statement)
            .map_or(0, |at| sent[..at].chars().count());
        let executed = Executed {
            offset,
            sql: sent,
            shift,
        };
        let options = ExecOptions {
            cache: CachePolicy::Refresh,
            page: None,
        };
        self.start(tab_id, connection, executed, options, false);
    }

    /// Sends a run and marks its tab busy.
    fn start(
        &mut self,
        tab_id: TabId,
        connection: ConnectionId,
        executed: Executed,
        options: ExecOptions,
        append: bool,
    ) {
        let Some(tab) = self.workspace.get_mut(tab_id) else {
            return;
        };
        let request = self
            .backend
            .execute(connection, executed.sql.clone(), options);
        if !append {
            // New rows: changes to the old ones are gone with them.
            tab.edits.clear();
            tab.source = match &tab.kind {
                TabKind::Data(view) => Some(sql::TableRef {
                    schema: Some(sql::Name {
                        text: view.schema.clone(),
                        quoted: true,
                    }),
                    name: sql::Name {
                        text: view.relation.clone(),
                        quoted: true,
                    },
                }),
                // A sorted query wraps the one written, which is the one
                // that tells the table.
                TabKind::Script => {
                    sql::single_table_select(tab.unsorted.as_deref().unwrap_or(&executed.sql))
                }
            };
            tab.paging = options.page.map(|_| Paging {
                base: executed.sql.clone(),
                options,
                loaded: 0,
                has_more: false,
                total: None,
            });
            tab.last_run = Some((executed.clone(), options));
        }
        tab.running = Some(Running {
            request,
            started: Instant::now(),
            executed,
            cancelling: false,
            append,
        });
    }

    /// Reads a data tab's first page with its filter and sort.
    fn run_data(&mut self, ctx: &egui::Context, tab_id: TabId, cache: CachePolicy) {
        if self.keeps_edits(ctx, tab_id) {
            return;
        }
        let Some(tab) = self.workspace.get_mut(tab_id) else {
            return;
        };
        let (Some(connection), Some(view)) = (tab.connection, tab.data()) else {
            return;
        };
        if tab.running.is_some() {
            return;
        }
        if !self.backend.is_connected(connection) {
            self.queued.push((tab_id, RunScope::Data));
            self.connect(connection, None);
            return;
        }
        let filter = view.filter.trim().to_owned();
        if !sql::is_single_expression(&filter) {
            self.toasts.push(
                ctx,
                Toast::error(
                    "The filter must be one condition",
                    "Use a WHERE expression such as id > 10 AND name ILIKE 'a%', without ';' or unbalanced brackets.",
                ),
            );
            return;
        }
        let Some(sql) = self.backend.browse_sql(
            connection,
            &view.schema,
            &view.relation,
            Some(&filter),
            view.order.as_ref(),
        ) else {
            return;
        };
        if let Some(view) = tab.data_mut() {
            view.applied = filter;
        }
        tab.sql.clone_from(&sql);
        let executed = Executed {
            offset: 0,
            sql,
            shift: 0,
        };
        let options = ExecOptions {
            cache,
            page: Some(Page::first()),
        };
        self.start(tab_id, connection, executed, options, false);
    }

    /// Reads the next page of a paged result, or all that remains up to the
    /// row cap.
    fn fetch_more(&mut self, tab_id: TabId, all: bool) {
        let max_rows = self.backend.max_rows() as u64;
        let Some(tab) = self.workspace.get_mut(tab_id) else {
            return;
        };
        let (Some(connection), Some(paging)) = (tab.connection, &tab.paging) else {
            return;
        };
        if tab.running.is_some() || !paging.has_more {
            return;
        }
        let limit = if all {
            max_rows.saturating_sub(paging.loaded)
        } else {
            Page::SIZE.min(max_rows.saturating_sub(paging.loaded))
        };
        if limit == 0 {
            return;
        }
        let options = ExecOptions {
            page: Some(Page {
                offset: paging.loaded,
                limit,
            }),
            ..paging.options
        };
        let executed = tab.last_run.as_ref().map_or_else(
            || Executed {
                offset: 0,
                sql: paging.base.clone(),
                shift: 0,
            },
            |(executed, _)| executed.clone(),
        );
        self.start(tab_id, connection, executed, options, true);
    }

    /// Runs the shown result again, past the cache.
    fn refresh(&mut self, ctx: &egui::Context, tab_id: TabId) {
        if self.keeps_edits(ctx, tab_id) {
            return;
        }
        let Some(tab) = self.workspace.get(tab_id) else {
            return;
        };
        if tab.data().is_some() {
            self.run_data(ctx, tab_id, CachePolicy::Refresh);
            return;
        }
        let (Some(connection), Some((executed, options))) = (tab.connection, tab.last_run.clone())
        else {
            return;
        };
        if tab.running.is_some() || !self.backend.is_connected(connection) {
            return;
        }
        // Changing every row again is asked for from the editor, not
        // repeated by a refresh.
        if !sql::unbounded_writes(&executed.sql).is_empty() {
            self.toasts.push(
                ctx,
                Toast::error(
                    "Not run again",
                    "It changes every row of a table. Run it from the editor to confirm it.",
                ),
            );
            return;
        }
        let cache = match options.cache {
            CachePolicy::Off => CachePolicy::Off,
            CachePolicy::Use | CachePolicy::Refresh => CachePolicy::Refresh,
        };
        let options = ExecOptions {
            cache,
            page: options.page.map(|_| Page::first()),
        };
        self.start(tab_id, connection, executed, options, false);
    }

    fn count_rows(&mut self, tab_id: TabId) {
        let Some(tab) = self.workspace.get_mut(tab_id) else {
            return;
        };
        let (Some(connection), Some(paging)) = (tab.connection, &mut tab.paging) else {
            return;
        };
        if matches!(paging.total, Some(Count::Running(_))) {
            return;
        }
        let request = self.backend.count(connection, paging.base.clone());
        paging.total = Some(Count::Running(request));
    }

    // ---- actions ----------------------------------------------------------

    fn apply_actions(&mut self, ctx: &egui::Context) {
        let actions = std::mem::take(&mut self.actions);
        for action in actions {
            self.apply(ctx, action);
        }
    }

    #[allow(clippy::too_many_lines)]
    #[hotpath::measure]
    fn apply(&mut self, ctx: &egui::Context, action: Action) {
        match action {
            Action::NewConnection => {
                if !self.new_connection() {
                    // Nothing installed and no catalogue yet: open once it
                    // arrives.
                    self.providers.awaiting_new_connection = true;
                    if !matches!(self.providers.listing, providers::Listing::Loading) {
                        self.load_providers();
                    }
                }
            }
            Action::RestartToUpdate => {
                let Some(updates) = &self.updates else {
                    return;
                };
                match updates.restart() {
                    // The helper installs once Violet has quit.
                    Ok(()) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                    Err(error) => self.toasts.push(
                        ctx,
                        Toast::error("The update could not start", format!("{error:#}")),
                    ),
                }
            }
            Action::OpenUrl(url) => ctx.open_url(egui::OpenUrl::new_tab(url)),
            Action::OpenProviders => {
                self.providers.open = true;
                self.load_providers();
            }
            Action::RefreshProviders => self.load_providers(),
            Action::InstallProvider(id) => {
                self.providers.errors.remove(&id);
                self.providers.busy.insert(id.clone());
                self.backend.install_provider(id);
            }
            Action::RemoveProvider(id) => {
                self.providers.errors.remove(&id);
                self.providers.busy.insert(id.clone());
                self.backend.remove_provider(id);
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
                        self.disconnect_opened(id);
                        let mut gone = self.explorer.opened_under(id);
                        gone.push(id);
                        for gone in gone {
                            self.explorer.remove(gone);
                            self.explorer.opened.remove(&gone);
                            for tab in &mut self.workspace.tabs {
                                if tab.connection == Some(gone) {
                                    tab.connection = None;
                                }
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
                self.disconnect_opened(id);
                self.explorer.links.insert(id, Link::Offline);
                self.backend.disconnect(id);
            }
            Action::OpenDatabase(parent, database) => self.open_database(parent, database),
            Action::DatabaseTask(parent, database, kind) => {
                if let Some(id) = self.database_session(parent, &database) {
                    self.open_task(id, kind, Vec::new());
                }
            }
            Action::Refresh(id) => {
                self.explorer.forget(id);
                self.explorer.schemas.insert(id, Loadable::Loading);
                self.backend.load_schemas(id);
                if self.shows_all_databases(id) {
                    self.explorer.databases.insert(id, Loadable::Loading);
                    self.backend.load_databases(id);
                }
            }
            Action::LoadObjects(id, schema) => self.load_objects(id, schema),
            Action::LoadColumns(id, schema, relation) => self.load_columns(id, schema, relation),
            Action::LoadForCompletion(id, needs) => self.load_for_completion(id, needs),
            Action::SaveProfile {
                profile,
                password,
                connect,
            } => {
                let id = profile.id;
                let reconnect = self.backend.is_connected(id);
                match self.profiles.upsert(profile, password.as_ref()) {
                    Ok(()) => {
                        self.names_unchecked = true;
                        // Other databases open on the old settings close; they
                        // reopen from the tree.
                        self.disconnect_opened(id);
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
                self.workspace.open(title, connection, sql, TabKind::Script);
            }
            Action::OpenData {
                connection,
                schema,
                relation,
            } => {
                if let Some(tab) = self.workspace.data_tab(connection, &schema, &relation) {
                    self.workspace.active = Some(tab);
                    return;
                }
                let view = DataView::new(schema, relation.clone());
                let tab = self.workspace.open(
                    relation,
                    Some(connection),
                    String::new(),
                    TabKind::Data(view),
                );
                self.run_data(ctx, tab, CachePolicy::Use);
            }
            Action::Run(tab, scope) => self.run(ctx, tab, scope),
            action @ (Action::RunConfirmed(..)
            | Action::EditCell(..)
            | Action::SetCell(..)
            | Action::RevertCell(..)
            | Action::DiscardEdits(_)
            | Action::ReviewEdits(_)
            | Action::ApplyEdits(_)) => self.edit_action(ctx, action),
            action @ (Action::SaveScript(_)
            | Action::SaveScriptAs(_)
            | Action::SaveScriptNamed(..)
            | Action::OpenScriptFile
            | Action::OpenScript(_)) => self.script_action(ctx, action),
            Action::RefreshTab(tab) => self.refresh(ctx, tab),
            Action::ApplyFilter(tab) => self.run_data(ctx, tab, CachePolicy::Use),
            Action::ClearFilter(tab) => {
                if let Some(view) = self.workspace.get_mut(tab).and_then(|t| t.data_mut()) {
                    view.filter.clear();
                }
                self.run_data(ctx, tab, CachePolicy::Use);
            }
            Action::SortBy(tab, column, descending) => self.sort(ctx, tab, column, descending),
            Action::FetchMore(tab) => self.fetch_more(tab, false),
            Action::FetchAll(tab) => self.fetch_more(tab, true),
            Action::CountRows(tab) => self.count_rows(tab),
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
            Action::Export(tab_id, format) => self.export(ctx, tab_id, format),
            Action::AdvancedCopy(tab_id) => self.open_copy_dialog(tab_id),
            Action::RememberCopy(format, headers) => self.copy_choice = (format, headers),
            Action::CloseReference(tab_id) => {
                if let Some(tab) = self.workspace.get_mut(tab_id) {
                    tab.grid_selection.clear();
                }
                self.reference = None;
            }
            Action::Copy(text) => {
                ctx.copy_text(text);
                self.toasts.push(ctx, Toast::success("Copied to clipboard"));
            }
            Action::OpenHistory => {
                self.history_view = Some(history::HistoryView::default());
            }
            Action::ClearHistory => match self.history_store.clear() {
                Ok(()) => self.history.clear(),
                Err(error) => self.toasts.push(
                    ctx,
                    Toast::error("Could not clear the history", error.to_string()),
                ),
            },
            Action::InsertSql(text) => self.insert_sql(ctx, &text),
            action @ (Action::OpenTask { .. }
            | Action::TrustTools(_)
            | Action::TaskFindPrograms
            | Action::TaskPreview
            | Action::TaskInspect(_)
            | Action::TaskStart
            | Action::TaskCancel
            | Action::TaskAgain
            | Action::TaskClose
            | Action::RetryTools(_)
            | Action::Reveal(_)) => self.task_action(ctx, action),
        }
    }

    /// Puts `text` at the active editor's cursor, replacing the selection,
    /// on lines of its own.
    fn insert_sql(&mut self, ctx: &egui::Context, text: &str) {
        let Some(tab) = self
            .workspace
            .active
            .and_then(|id| self.workspace.get_mut(id))
            .filter(|tab| tab.data().is_none())
        else {
            return;
        };
        let range = tab.selection.start.min(tab.sql.len())..tab.selection.end.min(tab.sql.len());
        let mut block = text.trim().to_owned();
        if !block.ends_with(';') {
            block.push(';');
        }
        let before = &tab.sql[..range.start];
        if !before.is_empty() && !before.ends_with('\n') {
            block.insert_str(0, "\n\n");
        }
        block.push('\n');
        tab.sql.replace_range(range.clone(), &block);
        let end = range.start + block.len();
        tab.selection = end..end;
        let caret = tab.sql[..end].chars().count();
        editor::place_cursor(ctx, tab.id, caret);
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        let modal_open = self.form.is_some()
            || self.prompt.is_some()
            || self.script_prompt.is_some()
            || self.cell_editor.is_some()
            || self.edit_review.is_some()
            || self.run_review.is_some()
            || self.confirm_delete.is_some()
            || self.history_view.is_some()
            || self.task_dialog.is_some();
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
            if input.consume_shortcut(&shortcuts::EXPLAIN_ANALYZE)
                && let Some(tab) = active
            {
                self.actions
                    .push(Action::Run(tab, RunScope::ExplainAnalyze));
            }
            if input.consume_shortcut(&shortcuts::EXPLAIN)
                && let Some(tab) = active
            {
                self.actions.push(Action::Run(tab, RunScope::Explain));
            }
            // Save as first: it is save plus Shift.
            if input.consume_shortcut(&shortcuts::SAVE_AS)
                && let Some(tab) = active
            {
                self.actions.push(Action::SaveScriptAs(tab));
            }
            if input.consume_shortcut(&shortcuts::SAVE)
                && let Some(tab) = active
            {
                self.actions.push(Action::SaveScript(tab));
            }
            if input.consume_shortcut(&shortcuts::OPEN) {
                self.actions.push(Action::OpenScriptFile);
            }
            if input.consume_shortcut(&shortcuts::HISTORY) {
                self.actions.push(Action::OpenHistory);
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
                let connections = connections(&self.profiles, &self.explorer);
                chrome::tab_strip(ui, &self.workspace, &connections, &mut self.actions);
            });

        let Some(active) = self.workspace.active else {
            return;
        };
        let editing = self.editing(active);
        let links = self.sync_reference(active);
        let profiles = &*connections(&self.profiles, &self.explorer);
        let explorer = &self.explorer;
        let actions = &mut self.actions;
        let reference = self.reference.as_mut().filter(|r| r.tab == active);
        let Some(tab) = self.workspace.tabs.iter_mut().find(|tab| tab.id == active) else {
            return;
        };
        // Completion quotes names the way the connection's database does.
        let dialect = tab
            .connection
            .and_then(|id| profiles.iter().find(|profile| profile.id == id))
            .and_then(|profile| self.backend.registry().get(&profile.driver))
            .map_or(violet_core::SqlDialect::ANSI, |driver| {
                driver.info().dialect
            });

        Panel::top(Id::new(("toolbar", tab.id)))
            .exact_size(44.0)
            .frame(Frame::NONE.fill(palette.window))
            .show_separator_line(false)
            .show(ui, |ui| {
                chrome::toolbar(ui, tab, profiles, explorer, actions)
            });

        if let Some(reference) = reference {
            Panel::right("reference")
                .resizable(true)
                .default_size(320.0)
                .size_range(240.0..=600.0)
                .frame(
                    Frame::NONE
                        .fill(palette.panel)
                        .stroke(Stroke::new(1.0, palette.outline)),
                )
                .show(ui, |ui| {
                    ui.set_min_height(ui.available_height());
                    reference::show(ui, reference, actions);
                });
        }

        // A data tab is all grid, under its filter bar.
        if tab.data().is_some() {
            Panel::top(Id::new(("filter", tab.id)))
                .exact_size(40.0)
                .frame(
                    Frame::NONE
                        .fill(palette.window)
                        .stroke(Stroke::new(1.0, palette.outline)),
                )
                .show_separator_line(false)
                .show(ui, |ui| chrome::filter_bar(ui, tab, actions));
            egui::CentralPanel::default()
                .frame(Frame::NONE.fill(palette.window))
                .show(ui, |ui| {
                    results::show(ui, tab, &editing, &links.columns, actions)
                });
            return;
        }

        // One results panel for every editor, so they all keep the size the
        // user gave it.
        let total = ui.available_height();
        let results = Panel::bottom("results")
            .resizable(true)
            .default_size(total * self.settings.results_share.clamp(0.1, 0.9))
            .size_range(90.0..=(total - 80.0).max(120.0))
            .frame(
                Frame::NONE
                    .fill(palette.window)
                    .stroke(Stroke::new(1.0, palette.outline)),
            )
            .show(ui, |ui| {
                // The panel remembers the height its contents take: they
                // fill it, or it would shrink to a smaller result each run.
                ui.set_min_height(ui.available_height());
                results::show(ui, tab, &editing, &links.columns, actions);
            });
        self.layout.results = (total > 0.0).then(|| results.response.rect.height() / total);

        let search = tab
            .connection
            .map(|id| explorer.search_path(id))
            .unwrap_or_default();
        let quote = |name: &str| violet_core::Dialect::quote_ident(&dialect, name);
        let assist = editor::Assist {
            connection: tab.connection,
            catalog: &ConnectionCatalog {
                explorer,
                id: tab.connection,
            },
            options: completion::Options {
                search: &search,
                quote: &quote,
            },
        };
        egui::CentralPanel::default()
            .frame(Frame::NONE.fill(palette.window))
            .show(ui, |ui| editor::show(ui, tab, &assist, actions));
    }

    fn modals(&mut self, ctx: &egui::Context) {
        if let Some(form) = &mut self.form {
            let drivers = self.providers.choices(self.backend.registry().infos());
            if !connection_form::show(ctx, form, &drivers, &mut self.actions) {
                self.form = None;
            }
        }
        if self.providers.open
            && !providers::show(ctx, &self.providers, &self.providers_dir, &mut self.actions)
        {
            self.providers.open = false;
        }
        if let Some(prompt) = &mut self.prompt
            && !connection_form::show_prompt(ctx, prompt, &mut self.actions)
        {
            self.prompt = None;
        }
        if let Some(prompt) = &mut self.script_prompt
            && !script_flow::show_prompt(ctx, prompt, &mut self.actions)
        {
            self.script_prompt = None;
        }
        if let Some(editor) = &mut self.cell_editor
            && !edit_flow::show_cell_editor(ctx, editor, &mut self.actions)
        {
            self.cell_editor = None;
        }
        if let Some(dialog) = &mut self.copy_dialog
            && !copy_flow::show(ctx, dialog, &mut self.actions)
        {
            self.copy_dialog = None;
        }
        if let Some(review) = &self.edit_review
            && !edit_flow::show_edit_review(ctx, review, &mut self.actions)
        {
            self.edit_review = None;
        }
        if let Some(review) = &self.run_review
            && !edit_flow::show_run_review(ctx, review, &mut self.actions)
        {
            self.run_review = None;
        }
        if let Some(view) = &mut self.history_view {
            let active = self.workspace.active();
            let target = history::Target {
                editor: active.is_some_and(|tab| tab.data().is_none()),
                connection: active.and_then(|tab| tab.connection),
            };
            if !history::show(ctx, view, &self.history, &target, &mut self.actions) {
                self.history_view = None;
            }
        }
        self.task_modal(ctx);
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
    #[cfg(debug_assertions)]
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        if let Some(snapshot) = &mut self.snapshot {
            snapshot.inject(raw_input);
        }
    }

    #[hotpath::measure]
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_events(ctx);
        self.shortcuts(ctx);
        if std::mem::take(&mut self.names_unchecked) {
            for profile in self.profiles.all() {
                self.fonts.cover(ctx, &profile.name);
            }
        }
        if let Some(tab) = self.workspace.active() {
            self.fonts.cover_editor(ctx, &tab.sql);
        }
        self.fonts.poll(ctx);
        let closing = ctx.input(|input| input.viewport().close_requested());
        self.save_session(closing);
    }

    #[hotpath::measure]
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
                let update = self
                    .updates
                    .as_ref()
                    .map(crate::update::Updates::status)
                    .unwrap_or_default();
                chrome::title_bar(
                    ui,
                    &title,
                    self.settings.theme.into(),
                    &update,
                    &mut self.actions,
                );
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
                    &connections(&self.profiles, &self.explorer),
                    &self.explorer,
                );
            });

        let mut sidebar_open = self.settings.sidebar;
        let sidebar = Panel::left("sidebar")
            .resizable(true)
            .default_size(self.settings.sidebar_width)
            .size_range(200.0..=520.0)
            .frame(
                Frame::NONE
                    .fill(palette.panel)
                    .stroke(Stroke::new(1.0, palette.outline)),
            )
            .show_collapsible(ui, &mut sidebar_open, |ui| {
                // Filled, so the panel keeps its width (see the results).
                ui.set_min_height(ui.available_height());
                sidebar::show(
                    ui,
                    self.profiles.all(),
                    &self.script_list,
                    &mut self.explorer,
                    &mut self.actions,
                );
            });
        if std::mem::take(&mut self.explorer.picked) {
            self.follow_tree(&ctx);
        }
        self.layout.sidebar = sidebar.map(|sidebar| sidebar.response.rect.width());
        if sidebar_open != self.settings.sidebar {
            self.actions.push(Action::ToggleSidebar);
        }

        self.layout.results = None;
        egui::CentralPanel::default()
            .frame(Frame::NONE.fill(palette.window))
            .show(ui, |ui| self.central(ui));
        self.remember_layout(&ctx);

        self.modals(&ctx);
        self.toasts.show(&ctx);
        #[cfg(debug_assertions)]
        self.drive_snapshot(&ctx);
        self.apply_actions(&ctx);
        // The window is up: the helper may keep the update.
        if let Some(receipt) = self.update_receipt.take() {
            crate::update::acknowledge(receipt);
        }
    }
}

/// An error notice, for start-up problems found before the app exists.
pub fn toast_error(title: impl Into<String>, body: impl Into<String>) -> Toast {
    Toast::error(title, body)
}

fn fingerprint(session: &SavedSession) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    session.hash(&mut hasher);
    hasher.finish()
}
