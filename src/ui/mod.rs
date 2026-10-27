//! The egui interface. [`VioletApp`] owns the interface state, draws it,
//! turns what the user does into [`action::Action`]s, and applies them
//! against the application layer.

mod action;
mod agent_flow;
mod ai_flow;
mod catalog_flow;
mod cells;
mod chrome;
mod connection_form;
mod copy_flow;
mod definition_flow;
#[cfg(debug_assertions)]
mod devtools;
mod edit_flow;
mod editor;
mod export_flow;
mod fonts;
mod health_flow;
mod hints_flow;
mod history;
pub mod icons;
mod key_flow;
mod keys_flow;
mod mcp_flow;
mod metrics_flow;
mod plan_view;
mod project_flow;
mod providers;
mod reference;
mod rename_flow;
mod results;
mod script_flow;
mod selection;
mod shortcuts;
mod sidebar;
mod state;
mod stats_flow;
mod task_flow;
mod tasks;
pub mod theme;
mod toasts;
mod transaction_flow;
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
use violet_core::{ConnectionId, ConnectionProfile, DriverId, ErrorKind, Language, Page};
use violet_storage::{JsonFile, SavedScript, ScriptFolder, ScriptStore};

use crate::completion::{self, Need};
use crate::dialect::Explain;
use crate::settings::Settings;
use crate::sql;
use action::{Action, ExplainFlag, RunScope};
use connection_form::{ConnectionForm, PasswordPrompt, TestState};
use shortcuts::Command;
use state::{ConnectionCatalog, Explorer, Link, LoadKey, Loadable, NodeKey};
use theme::Palette;
use toasts::{Toast, Toasts};
use workspace::{
    Count, DataView, Executed, Outcome, Paging, QueryTab, Running, SavedSession, TabId, TabKind,
    Workspace,
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
    /// Where each workspace keeps its files.
    pub dirs: violet_storage::AppDirs,
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
    /// The folders below databases' scripts, as last listed.
    script_folders: Vec<ScriptFolder>,
    script_prompt: Option<script_flow::ScriptPrompt>,
    /// Asks about unsaved scripts before their tabs close.
    close_confirm: Option<script_flow::CloseConfirm>,
    /// The window closes once unsaved scripts were saved or discarded.
    quitting: bool,
    /// Asks what becomes of open transactions.
    tx_confirm: Option<transaction_flow::TxConfirm>,
    /// What waits for each connection's commit or rollback to finish.
    ending: HashMap<ConnectionId, transaction_flow::Then>,
    /// Open transactions were dealt with: the window may close.
    tx_settled: bool,
    keys_dialog: Option<keys_flow::KeysDialog>,
    ai_dialog: Option<ai_flow::AiDialog>,
    agent: agent_flow::AgentPanel,
    cell_editor: Option<edit_flow::CellEditor>,
    edit_review: Option<edit_flow::EditReview>,
    run_review: Option<edit_flow::RunReview>,
    /// Grid changes being written.
    applying: HashMap<violet_app::RequestId, edit_flow::Applying>,
    /// The rows the foreign keys followed from a tab's grid point at.
    reference: Option<reference::Reference>,
    /// Exports waiting for every row to be read.
    exports: HashMap<violet_app::RequestId, export_flow::PendingExport>,
    copy_dialog: Option<copy_flow::CopyDialog>,
    /// Definitions being read.
    definitions: HashMap<violet_app::RequestId, definition_flow::PendingDefinition>,
    /// Folder listings being read.
    folder_requests: HashMap<violet_app::RequestId, (ConnectionId, state::Folder)>,
    chooser: Option<catalog_flow::Chooser>,
    key_dialog: Option<key_flow::KeyDialog>,
    rename_prompt: Option<rename_flow::RenamePrompt>,
    drop_prompt: Option<rename_flow::DropPrompt>,
    rename_requests: HashMap<violet_app::RequestId, rename_flow::Pending>,
    /// Commands the tree ran on keys, waiting for their answer.
    key_requests: HashMap<violet_app::RequestId, key_flow::Pending>,
    /// A table's statistics, when the dialog is open.
    table_stats: Option<stats_flow::StatsView>,
    /// The server health panel, when open.
    health: Option<health_flow::Health>,
    /// What the panel asked, by request.
    health_requests: HashMap<violet_app::RequestId, health_flow::Asked>,
    /// Index lists being read: the table they are of.
    index_requests: HashMap<violet_app::RequestId, (ConnectionId, String, String)>,
    /// Pings sent, by the connection they measure.
    ping_requests: HashMap<violet_app::RequestId, ConnectionId>,
    /// Actions to run once their connection opens.
    waiting: Vec<(ConnectionId, Action)>,
    /// The format and header choice advanced copy starts with.
    copy_choice: (crate::export::Format, bool),
    confirm_delete: Option<ConnectionId>,
    /// A workspace waiting for the user to confirm its deletion.
    confirm_project_delete: Option<String>,
    dirs: violet_storage::AppDirs,
    /// A connection string typed on the welcome screen.
    welcome_uri: String,
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
    /// Completion items accepted lately, per connection; kept in memory.
    completion_history: HashMap<ConnectionId, completion::History>,
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

/// An analyzed write that failed halfway left its transaction, or the
/// savepoint it ran in, open: closes it.
fn undo_explain(backend: &violet_app::Backend, connection: ConnectionId, sent: &str) {
    let undo =
        if sent.starts_with("BEGIN;\nEXPLAIN") || sent.starts_with("START TRANSACTION;\nEXPLAIN") {
            "ROLLBACK"
        } else if sent.starts_with("SAVEPOINT violet_explain;") {
            "ROLLBACK TO SAVEPOINT violet_explain; RELEASE SAVEPOINT violet_explain"
        } else {
            return;
        };
    let options = ExecOptions {
        cache: CachePolicy::Refresh,
        page: None,
    };
    backend.execute_script(
        connection,
        None,
        undo.to_owned(),
        options,
        sql::transaction_control(undo),
    );
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
        mut settings: Settings,
        startup_errors: Vec<Toast>,
    ) -> Self {
        // Shortcuts no longer understood are dropped.
        settings
            .shortcuts
            .retain(|_, text| shortcuts::parse(text).is_some());
        shortcuts::apply(&settings.shortcuts);
        let schema_filters = settings.schema_filters.clone();
        let database_filters = settings.database_filters.clone();
        let mut app = Self {
            backend: services.backend,
            profiles: services.profiles,
            settings_file: services.settings,
            settings,
            explorer: Explorer {
                schema_filters,
                database_filters,
                ..Explorer::default()
            },
            workspace: Workspace::default(),
            toasts: Toasts::default(),
            form: None,
            providers: providers::Providers::default(),
            providers_dir: services.providers_dir,
            prompt: None,
            scripts: services.scripts,
            script_list: Vec::new(),
            script_folders: Vec::new(),
            script_prompt: None,
            close_confirm: None,
            quitting: false,
            tx_confirm: None,
            ending: HashMap::new(),
            tx_settled: false,
            keys_dialog: None,
            ai_dialog: None,
            agent: agent_flow::AgentPanel::default(),
            cell_editor: None,
            edit_review: None,
            run_review: None,
            applying: HashMap::new(),
            reference: None,
            exports: HashMap::new(),
            copy_dialog: None,
            definitions: HashMap::new(),
            waiting: Vec::new(),
            folder_requests: HashMap::new(),
            chooser: None,
            key_dialog: None,
            rename_prompt: None,
            drop_prompt: None,
            rename_requests: HashMap::new(),
            key_requests: HashMap::new(),
            ping_requests: HashMap::new(),
            index_requests: HashMap::new(),
            table_stats: None,
            health: None,
            health_requests: HashMap::new(),
            copy_choice: (crate::export::Format::Tsv, true),
            confirm_delete: None,
            confirm_project_delete: None,
            dirs: services.dirs,
            welcome_uri: String::new(),
            queued: Vec::new(),
            actions: Vec::new(),
            fonts: fonts::Fallbacks::default(),
            names_unchecked: true,
            history_store: services.history,
            history: Vec::new(),
            history_view: None,
            completion_history: HashMap::new(),
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
            Some(NodeKey::Folder(id, _)) => Some(id),
            Some(
                NodeKey::Scripts(_) | NodeKey::ScriptFolder(..) | NodeKey::ConnectionFolder(_),
            )
            | None => None,
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
            && !tab.has_changes()
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

    /// The id of a connection's driver, such as `postgres`.
    fn driver_of(&self, id: ConnectionId) -> Option<String> {
        connections(&self.profiles, &self.explorer)
            .iter()
            .find(|profile| profile.id == id)
            .map(|profile| profile.driver.as_str().to_owned())
    }

    /// What a connection's statements are written in.
    fn language(&self, id: ConnectionId) -> Language {
        connections(&self.profiles, &self.explorer)
            .iter()
            .find(|profile| profile.id == id)
            .map_or(Language::Sql, |profile| {
                self.explorer.language(profile.driver.as_str())
            })
    }

    /// The language of a tab's connection; SQL without one.
    fn tab_language(&self, tab_id: TabId) -> Language {
        self.workspace
            .get(tab_id)
            .and_then(|tab| tab.connection)
            .map_or(Language::Sql, |id| self.language(id))
    }

    /// Schemas whose objects need no qualifier on a connection.
    fn search_path(&self, id: ConnectionId) -> Vec<String> {
        self.explorer.search_path(id, self.family(id))
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
    /// Disconnects `id` and the databases opened under it.
    fn disconnect(&mut self, id: ConnectionId) {
        self.disconnect_opened(id);
        self.explorer.links.insert(id, Link::Offline);
        self.backend.disconnect(id);
    }

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
                let (ready, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut self.waiting)
                    .into_iter()
                    .partition(|(connection, _)| *connection == id);
                self.waiting = waiting;
                self.actions
                    .extend(ready.into_iter().map(|(_, action)| action));
                self.explorer.links.insert(id, Link::Online(server));
                self.explorer.forget(id);
                self.explorer
                    .metrics
                    .entry(id)
                    .or_default()
                    .connected(Instant::now());
                self.ping(id);
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
                if let Some(metrics) = self.explorer.metrics.get_mut(&id) {
                    metrics.connect_failed();
                }
                self.waiting.retain(|(connection, _)| *connection != id);
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
                if let Some(metrics) = self.explorer.metrics.get_mut(&id) {
                    metrics.closed();
                }
                self.explorer.links.insert(id, Link::Offline);
                self.explorer.forget(id);
            }
            Event::Transaction { id, state } => {
                self.explorer.transactions.insert(id, state);
            }
            Event::TransactionDone {
                id,
                command,
                result,
            } => self.transaction_done(ctx, id, command, result),
            Event::Databases { id, result } => {
                self.explorer
                    .databases
                    .insert(id, Loadable::from_result(result));
            }
            Event::Schemas { id, result } => {
                if self.language(id) == Language::Redis {
                    self.keyspaces_listed(id, &result);
                } else if let Ok(schemas) = &result {
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
                    .objects_loaded(id, schema, result.map(Into::into), Instant::now());
            }
            Event::Columns {
                id,
                schema,
                relation,
                result,
            } => {
                self.explorer.columns_loaded(
                    id,
                    schema,
                    relation,
                    result.map(Into::into),
                    Instant::now(),
                );
                self.explorer.hint_rev += 1;
            }
            Event::Entries {
                id,
                schema,
                pattern,
                more,
                result,
            } => self.keys_listed(id, schema, pattern, more, result),
            Event::KeysMatching {
                id,
                schema,
                prefix,
                result,
            } => {
                // A failed lookup counts as done, so it is not asked again.
                let keys = result.map(|page| page.objects).unwrap_or_default();
                self.explorer
                    .key_matches
                    .insert((id, schema, prefix), Loadable::Ready(keys.into()));
            }
            Event::ForeignKeys { id, schema, result } => {
                self.explorer.foreign_keys_loaded(
                    id,
                    schema,
                    result.map(Into::into),
                    Instant::now(),
                );
            }
            Event::QueryFinished { request, result } => {
                if let Some(pending) = self.rename_requests.remove(&request) {
                    self.renamed(ctx, pending, &result);
                    return;
                }
                if self.table_stats_read(request, &result) {
                    return;
                }
                if let Some(asked) = self.health_requests.remove(&request) {
                    self.health_answered(ctx, asked, result);
                    return;
                }
                if let Some(table) = self.index_requests.remove(&request) {
                    self.indexes_read(table, &result);
                    return;
                }
                if let Some(connection) = self.ping_requests.remove(&request) {
                    self.pinged(ctx, connection, &result);
                    return;
                }
                if let Some(pending) = self.key_requests.remove(&request) {
                    self.key_command_done(ctx, pending, &result);
                    return;
                }
                if let Some(applying) = self.applying.remove(&request) {
                    self.edits_applied(ctx, applying, &result);
                    return;
                }
                if let Some(folder) = self.folder_requests.remove(&request) {
                    self.folder_read(folder, result);
                    return;
                }
                if let Some(pending) = self.definitions.remove(&request) {
                    self.definition_read(ctx, pending, result);
                    return;
                }
                if let Some(pending) = self.exports.remove(&request) {
                    self.export_read(ctx, pending, result);
                    return;
                }
                if self.awaits_reference(request) {
                    self.reference_finished(request, result);
                    return;
                }
                let Some(tab) = self.workspace.by_request(request) else {
                    return;
                };
                let Some(running) = tab.running.take() else {
                    return;
                };
                let connection = tab.connection;
                let language = connection.map_or(Language::Sql, |id| {
                    connections(&self.profiles, &self.explorer)
                        .iter()
                        .find(|profile| profile.id == id)
                        .map_or(Language::Sql, |profile| {
                            self.explorer.language(profile.driver.as_str())
                        })
                });
                // DDL that ran leaves the loaded catalog behind.
                let ddl = connection.filter(|_| {
                    catalog_flow::run_reloads_catalog(
                        language,
                        &running.executed.sql,
                        result.is_ok(),
                    )
                });
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
                    sql: crate::lang::redact_secrets(language, &running.executed.sql),
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
                        if crate::lang::sets_secret(language, &running.executed.sql) {
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
                tab.received = Some(Instant::now());
                tab.render = None;
                tab.rtt = connection
                    .and_then(|id| self.explorer.metrics.get(&id))
                    .and_then(|metrics| metrics.latency.last());
                tab.outcome = Some(match result {
                    Ok(mut output) => {
                        // A key's profile is four answers: show it as one.
                        if language == Language::Redis
                            && running.executed.sql.starts_with("TYPE ")
                            && let Some(report) = crate::lang::redis_profile_report(&output.sets)
                        {
                            output.sets = vec![report];
                        }
                        tab.keep_plan(&running.executed.sql, &output.sets);
                        // Show the last set with rows: usually what was asked for.
                        tab.active_set = output
                            .sets
                            .iter()
                            .rposition(violet_core::ResultSet::has_rows)
                            .unwrap_or(0);
                        Outcome::Done { output }
                    }
                    Err(error) => {
                        if let Some(id) = connection {
                            undo_explain(&self.backend, id, &running.executed.sql);
                        }
                        if matches!(error.kind, ErrorKind::Connection | ErrorKind::NotConnected)
                            && let Some(id) = connection
                            && !self.backend.is_connected(id)
                        {
                            self.explorer.links.insert(id, Link::Offline);
                            self.explorer.metrics.entry(id).or_default().dropped();
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
                if let Some(id) = connection {
                    self.ping_if_stale(id);
                }
                if let Some(id) = ddl {
                    self.reload_catalog(id);
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

    /// Asks again for the loads that failed and whose wait is over, and has
    /// the next frame come when the next wait ends.
    fn retry_loads(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        for key in self.explorer.retries.due(now) {
            if !self.explorer.link(key.owner()).is_online() {
                self.explorer.abandon(key, "not connected");
                continue;
            }
            match key {
                LoadKey::Objects(id, schema) => self.backend.load_objects(id, schema),
                LoadKey::Columns(id, schema, relation) => {
                    self.backend.load_columns(id, schema, relation);
                }
                LoadKey::ForeignKeys(id, schema) => self.backend.load_foreign_keys(id, schema),
            }
        }
        if let Some(wait) = self.explorer.retries.next_due(now) {
            ctx.request_repaint_after(wait);
        }
    }

    /// Forgets what was loaded for a connection and loads it again.
    fn reload_catalog(&mut self, id: ConnectionId) {
        self.explorer.forget(id);
        self.explorer.schemas.insert(id, Loadable::Loading);
        self.backend.load_schemas(id);
        if self.shows_all_databases(id) {
            self.explorer.databases.insert(id, Loadable::Loading);
            self.backend.load_databases(id);
        }
    }

    fn load_objects(&mut self, id: ConnectionId, schema: String) {
        self.explorer.objects_loading(id, schema.clone());
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
                Need::Keys(schema) => {
                    let listed = self
                        .explorer
                        .keys
                        .get(&(id, schema.clone()))
                        .is_some_and(|keys| keys.listed || keys.loading);
                    if !listed {
                        self.load_keys(id, schema, false);
                    }
                }
                Need::KeysMatching(schema, prefix) => {
                    let key = (id, schema.clone(), prefix.clone());
                    if let std::collections::hash_map::Entry::Vacant(entry) =
                        self.explorer.key_matches.entry(key)
                    {
                        entry.insert(Loadable::Loading);
                        let glob = completion::key_glob(&prefix);
                        self.backend
                            .match_keys(id, schema, prefix, glob, completion::KEY_LOOKUP);
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
        self.explorer
            .metrics
            .entry(id)
            .or_default()
            .connecting(Instant::now());
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
        let language = tab.connection.map_or(Language::Sql, |id| {
            connections(&self.profiles, &self.explorer)
                .iter()
                .find(|profile| profile.id == id)
                .map_or(Language::Sql, |profile| {
                    self.explorer.language(profile.driver.as_str())
                })
        });
        let explain = matches!(scope, RunScope::Explain | RunScope::ExplainAnalyze);
        let range = match scope {
            RunScope::Script | RunScope::Data => 0..tab.sql.len(),
            RunScope::Statement | RunScope::Explain | RunScope::ExplainAnalyze
                if !tab.selection.is_empty() =>
            {
                tab.selection.clone()
            }
            RunScope::Statement | RunScope::Explain | RunScope::ExplainAnalyze => {
                match crate::lang::statement_at(language, &tab.sql, tab.selection.start) {
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
            if language != Language::Sql {
                self.explain_other(ctx, tab_id, connection, language, offset, &text, scope);
                return;
            }
            self.explain(ctx, tab_id, connection, offset, &text, scope);
            return;
        }
        if !confirmed {
            let writes = self.unbounded_writes(&text, connection, language);
            if !writes.is_empty() {
                self.run_review = Some(edit_flow::RunReview {
                    tab: tab_id,
                    scope,
                    writes,
                    language,
                });
                return;
            }
        }
        // A new run starts unsorted.
        if let Some(tab) = self.workspace.get_mut(tab_id) {
            tab.sort = None;
            tab.unsorted = None;
        }
        if language != Language::Sql {
            // Commands run as written, one reply each; nothing is cached.
            let executed = Executed {
                offset,
                sql: text,
                shift: 0,
            };
            self.start(tab_id, connection, executed, ExecOptions::WRITE, false);
            return;
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
            Some(single) => (single, Some(self.first_page())),
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
            page: Some(Page {
                offset: 0,
                limit: self.settings.fetch_size,
            }),
        };
        tab.sort = descending.map(|descending| (column, descending));
        tab.unsorted = descending.is_some().then_some(base);
        self.start(tab_id, connection, executed, options, false);
    }

    /// Whether `tab_id` has grid changes a new read would drop: the user
    /// saves or discards them first.
    fn keeps_edits(&mut self, ctx: &egui::Context, tab_id: TabId) -> bool {
        let pending = self
            .workspace
            .get(tab_id)
            .is_some_and(QueryTab::has_changes);
        if pending {
            self.toasts.push(
                ctx,
                Toast::error(
                    "The grid's changes are not saved",
                    "Save or discard them before reading the rows again.",
                ),
            );
        }
        pending
    }

    /// Explains a statement of a language that is not SQL: MongoDB's
    /// `explain()`, or what a Redis command's key weighs.
    #[allow(clippy::too_many_arguments)]
    fn explain_other(
        &mut self,
        ctx: &egui::Context,
        tab_id: TabId,
        connection: ConnectionId,
        language: Language,
        offset: usize,
        text: &str,
        scope: RunScope,
    ) {
        let analyze = scope == RunScope::ExplainAnalyze;
        let sent = match crate::lang::explain_script(language, text, analyze) {
            Ok(sent) => sent,
            Err(reason) => {
                self.toasts.push(ctx, Toast::info(reason));
                return;
            }
        };
        if analyze && language == Language::Mongodb && crate::mongo::plans_only(text) {
            self.toasts.push(
                ctx,
                Toast::info("MongoDB analyzes reads only; this shows the planner's choice"),
            );
        }
        let shift = sent.find(text).map_or(0, |at| sent[..at].chars().count());
        let executed = Executed {
            offset,
            sql: sent,
            shift,
        };
        self.start(tab_id, connection, executed, ExecOptions::WRITE, false);
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
        let statement = text.trim_end().trim_end_matches(';').trim_end();
        let family = self.family(connection);
        let mut options = self
            .workspace
            .get(tab_id)
            .map_or_else(Explain::default, |tab| tab.explain);
        options.analyze = analyze;
        // A write is analyzed inside a transaction that is rolled back, where
        // the database can undo it.
        let rolled_back = analyze
            && !sql::plan(text).reads_only
            && sql::is_row_write(text)
            && family
                .explain_rolled_back(statement, &options, false)
                .is_some();
        if analyze && !rolled_back && !sql::plan(text).reads_only {
            self.toasts.push(
                ctx,
                Toast::error(
                    "EXPLAIN ANALYZE runs the statement",
                    "Violet only analyses statements that read, or writes it can roll back (Postgres and MySQL). Use Explain to see the plan of one that writes.",
                ),
            );
            return;
        }
        if analyze && family == crate::dialect::Family::Sqlite {
            self.toasts.push(
                ctx,
                Toast::info("SQLite has no EXPLAIN ANALYZE; Explain shows its query plan"),
            );
            return;
        }
        // Where the database has a tree form, the results pane draws it.
        let open = self
            .explorer
            .transactions
            .get(&connection)
            .is_some_and(|state| state.open);
        let tree = if rolled_back {
            family.explain_rolled_back(statement, &options, open)
        } else {
            family.explain_tree(statement, &options)
        };
        let Some(sent) = tree.or_else(|| self.backend.explain_sql(connection, statement, analyze))
        else {
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
        // A key-value store's data view reads its own database, whatever
        // the editor selected.
        let schema = tab.data().map(|view| view.schema.clone());
        // Transaction statements the user wrote are followed, not doubled.
        let control = sql::transaction_control(&executed.sql);
        let request =
            self.backend
                .execute_script(connection, schema, executed.sql.clone(), options, control);
        if !append {
            // New rows: changes to the old ones are gone with them.
            tab.clear_changes();
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
        if view.subtype.is_none() && !sql::is_single_expression(&filter) {
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
            view.subtype.as_deref(),
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
            page: Some(self.first_page()),
        };
        self.start(tab_id, connection, executed, options, false);
    }

    fn first_page(&self) -> Page {
        Page {
            offset: 0,
            limit: self.settings.fetch_size,
        }
    }

    /// Reads the next page of a paged result, or all that remains up to the
    /// row cap.
    fn fetch_more(&mut self, ctx: &egui::Context, tab_id: TabId, all: bool) {
        let max_rows = self.backend.max_rows() as u64;
        let Some(tab) = self.workspace.get_mut(tab_id) else {
            return;
        };
        // Added rows follow the result's: more rows would land among them.
        if tab.added > 0 {
            self.toasts.push(
                ctx,
                Toast::error(
                    "The added rows are not saved",
                    "Save or discard them before reading more rows.",
                ),
            );
            return;
        }
        let (Some(connection), Some(paging)) = (tab.connection, &tab.paging) else {
            return;
        };
        if tab.running.is_some() || !paging.has_more {
            return;
        }
        let limit = if all {
            max_rows.saturating_sub(paging.loaded)
        } else {
            self.settings
                .fetch_size
                .min(max_rows.saturating_sub(paging.loaded))
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

    /// Formats an editor's selection, or its whole script, keeping the
    /// formatted text selected.
    fn format_sql(&mut self, ctx: &egui::Context, tab_id: TabId) {
        let family = self
            .workspace
            .get(tab_id)
            .and_then(|tab| tab.connection)
            .map_or_else(Default::default, |id| self.family(id));
        let Some(tab) = self.workspace.get_mut(tab_id) else {
            return;
        };
        let range = if tab.selection.is_empty() {
            0..tab.sql.len()
        } else {
            tab.selection.clone()
        };
        let Some(source) = tab.sql.get(range.clone()) else {
            return;
        };
        let formatted = match crate::format::format(source, family) {
            Ok(formatted) => formatted,
            Err(refused) => {
                let why = match refused {
                    crate::format::Refused::Backslash => {
                        "A string escapes a quote with a backslash, which the formatter does \
                         not read."
                    }
                    crate::format::Refused::Changed => {
                        "Formatting would change more than spacing here."
                    }
                };
                self.toasts
                    .push(ctx, Toast::error("The SQL was not formatted", why));
                return;
            }
        };
        if formatted == source {
            return;
        }
        let text = format!(
            "{}{formatted}{}",
            &tab.sql[..range.start],
            &tab.sql[range.end..]
        );
        let selection = if tab.selection.is_empty() {
            0..0
        } else {
            range.start..range.start + formatted.len()
        };
        editor::replace_text(ctx, tab, crate::editing::Edited { text, selection });
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
        let language = tab.connection.map_or(Language::Sql, |id| self.language(id));
        // A refresh reads again; a statement that writes (`CREATE DATABASE`,
        // `INSERT`, a Redis `SET`) runs again only from the editor.
        let reads = match language {
            Language::Sql => crate::sql::plan(&executed.sql).reads_only,
            _ => options.cache != CachePolicy::Off,
        };
        if !reads {
            self.toasts.push(
                ctx,
                Toast::info("Not run again: it changes data. Run it from the editor."),
            );
            return;
        }
        // Changing every row again is asked for from the editor, not
        // repeated by a refresh.
        if !crate::lang::dangers(language, &executed.sql).is_empty() {
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
            page: options.page.map(|_| self.first_page()),
        };
        self.start(tab_id, connection, executed, options, false);
    }

    fn count_rows(&mut self, tab_id: TabId) {
        let Some(tab) = self.workspace.get_mut(tab_id) else {
            return;
        };
        let schema = tab.data().map(|view| view.schema.clone());
        let (Some(connection), Some(paging)) = (tab.connection, &mut tab.paging) else {
            return;
        };
        if matches!(paging.total, Some(Count::Running(_))) {
            return;
        }
        let request = self.backend.count(connection, schema, paging.base.clone());
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
            Action::NewConnectionFromUri(mut text) => {
                let parsed = violet_core::uri::parse(&text);
                zeroize::Zeroize::zeroize(&mut text);
                match parsed {
                    Ok(parsed) => {
                        let choices = self.providers.choices(self.backend.registry().infos());
                        match choices.iter().find(|c| c.info.id.as_str() == parsed.driver) {
                            Some(choice) => {
                                let mut form = ConnectionForm::create(&choice.info);
                                form.apply_uri(parsed, &choice.info);
                                self.form = Some(form);
                            }
                            None => self.toasts.push(
                                ctx,
                                Toast::error(
                                    "No provider for that database",
                                    format!(
                                        "The {} provider is not on offer yet; open the providers \
                                         dialog to refresh the catalogue.",
                                        parsed.driver
                                    ),
                                ),
                            ),
                        }
                    }
                    Err(message) => self.toasts.push(
                        ctx,
                        Toast::error("Not a connection string Violet reads", message),
                    ),
                }
            }
            Action::RestartToUpdate => {
                let Some(updates) = &self.updates else {
                    return;
                };
                // The window closes once the helper has the update (see
                // `ui`); it installs it after Violet has quit.
                if let Err(error) = updates.restart() {
                    self.toasts.push(
                        ctx,
                        Toast::error("The update could not start", format!("{error:#}")),
                    );
                }
            }
            Action::RetryUpdate => {
                if let Some(updates) = &self.updates {
                    updates.retry();
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
            Action::MoveToFolder(id, folder) => {
                if let Err(error) = self.profiles.move_to_folder(id, &folder) {
                    self.toasts.push(
                        ctx,
                        Toast::error("Could not move the connection", error.to_string()),
                    );
                }
            }
            Action::MoveFolder(folder, parent) => {
                let to = violet_app::moved_folder(&folder, &parent);
                if let Some(to) = &to
                    && self.profiles.folders().contains(to)
                {
                    self.toasts
                        .push(ctx, Toast::info("There is a folder with this name there"));
                } else if let Some(to) = to
                    && let Err(error) = self.profiles.rename_folder(&folder, &to)
                {
                    self.toasts.push(
                        ctx,
                        Toast::error("Could not move the folder", error.to_string()),
                    );
                }
            }
            Action::DeleteFolder(folder) => {
                if let Err(error) = self.profiles.delete_folder(&folder) {
                    self.toasts.push(
                        ctx,
                        Toast::error("Could not delete the folder", error.to_string()),
                    );
                }
            }
            Action::Connect(id) => self.connect(id, None),
            Action::ConnectWithPassword(id, password) => self.connect(id, Some(password)),
            Action::Disconnect(id) => self.disconnect_asking(id),
            Action::Transact(id, command) => self.transact(id, command),
            Action::OpenDatabase(parent, database) => self.open_database(parent, database),
            Action::DatabaseTask(parent, database, kind) => {
                if let Some(id) = self.database_session(parent, &database) {
                    self.open_task(id, kind, Vec::new());
                }
            }
            Action::Refresh(id) => self.reload_catalog(id),
            Action::LoadObjects(id, schema) => self.load_objects(id, schema),
            Action::LoadKeys(id, schema, more) => self.load_keys(id, schema, more),
            Action::KeyDialog {
                connection,
                schema,
                key,
                op,
            } => self.open_key_dialog(connection, schema, key, op),
            Action::RunInSchema {
                connection,
                schema,
                script,
                done,
            } => self.run_in_schema(ctx, connection, schema, script, done),
            Action::LoadColumns(id, schema, relation) => self.load_columns(id, schema, relation),
            Action::LoadForCompletion(id, needs) => self.load_for_completion(id, needs),
            Action::CompletionAccepted(id, item) => {
                self.completion_history
                    .entry(id)
                    .or_default()
                    .accepted(&item);
            }
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
                subtype,
            } => {
                if let Some(tab) = self.workspace.data_tab(connection, &schema, &relation) {
                    self.workspace.active = Some(tab);
                    return;
                }
                let mut view = DataView::new(schema, relation.clone());
                view.subtype = subtype;
                let tab = self.workspace.open(
                    relation,
                    Some(connection),
                    String::new(),
                    TabKind::Data(view),
                );
                self.run_data(ctx, tab, CachePolicy::Use);
            }
            Action::Run(tab, scope) => self.run(ctx, tab, scope),
            Action::TableStats(connection, schema, table) => {
                self.open_table_stats(ctx, connection, schema, table);
            }
            Action::OpenHealth(connection) => self.open_health(ctx, connection),
            Action::HealthRefresh => self.refresh_health(),
            Action::HealthSection(index) => {
                if let Some(view) = &mut self.health {
                    view.current = index;
                }
                self.load_section(index);
            }
            Action::HealthStop(stop) => self.stop_session(ctx, stop),
            Action::LoadIndexes(connection, schema, table) => {
                self.load_indexes(connection, schema, table);
            }
            Action::ToggleExplain(tab, flag) => {
                if let Some(tab) = self.workspace.get_mut(tab) {
                    match flag {
                        ExplainFlag::Buffers => tab.explain.buffers = !tab.explain.buffers,
                        ExplainFlag::Timing => tab.explain.timing = !tab.explain.timing,
                    }
                }
            }
            action @ (Action::RunConfirmed(..)
            | Action::EditCell(..)
            | Action::SetCell(..)
            | Action::RevertCell(..)
            | Action::DiscardEdits(_)
            | Action::AddRow(_)
            | Action::DeleteRows(..)
            | Action::RestoreRows(..)
            | Action::ReviewEdits(_)
            | Action::ApplyEdits(_)) => self.edit_action(ctx, action),
            action @ (Action::SaveScript(_)
            | Action::SaveScriptAs(_)
            | Action::SaveScriptNamed(..)
            | Action::OpenScriptFile
            | Action::OpenScript(_)
            | Action::MoveScript(..)
            | Action::MoveScriptFolder(..)) => self.script_action(ctx, action),
            action @ (Action::SwitchProject(_)
            | Action::AskDeleteProject(_)
            | Action::DeleteProject(_)) => self.project_action(ctx, action),
            Action::RefreshTab(tab) => self.refresh(ctx, tab),
            Action::ApplyFilter(tab) => self.run_data(ctx, tab, CachePolicy::Use),
            Action::ClearFilter(tab) => {
                if let Some(view) = self.workspace.get_mut(tab).and_then(|t| t.data_mut()) {
                    view.filter.clear();
                }
                self.run_data(ctx, tab, CachePolicy::Use);
            }
            Action::SortBy(tab, column, descending) => self.sort(ctx, tab, column, descending),
            Action::FetchMore(tab) => self.fetch_more(ctx, tab, false),
            Action::FetchAll(tab) => self.fetch_more(ctx, tab, true),
            Action::SetFetchSize(size) => {
                self.settings.fetch_size = size.max(1);
                self.save_settings(ctx);
            }
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
            Action::CloseTab(id) => self.close_tabs(vec![id]),
            Action::CloseTabs(ids) => self.close_tabs(ids),
            Action::SelectTab(id) => self.workspace.active = Some(id),
            Action::FormatSql(id) => self.format_sql(ctx, id),
            Action::OpenShortcuts => self.keys_dialog = Some(keys_flow::KeysDialog::default()),
            Action::SetShortcut(command, shortcut) => self.set_shortcut(ctx, command, shortcut),
            Action::Find(id, replacing) => {
                if let Some(tab) = self.workspace.get_mut(id) {
                    editor::open_find(tab, replacing);
                }
            }
            Action::AskRename(target) => self.ask_rename(ctx, target),
            Action::RenameTo(name) => self.rename(ctx, name),
            Action::AskDropDatabase(connection, name) => {
                self.ask_drop_database(ctx, connection, name);
            }
            Action::DropDatabase => self.drop_database(ctx),
            Action::SetAutoRefresh(id, every) => {
                if let Some(tab) = self.workspace.get_mut(id) {
                    tab.auto_refresh = every.map(|every| (every, Instant::now() + every));
                }
            }
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
            Action::OpenDefinition {
                connection,
                schema,
                name,
                kind,
            } => self.open_definition(ctx, connection, schema, name, kind),
            Action::OpenTrigger {
                connection,
                schema,
                table,
                name,
            } => self.open_trigger(ctx, connection, schema, table, name),
            Action::LoadFolder(connection, folder) => self.load_folder(connection, folder),
            Action::ChooseSchemas(session) => self.choose_schemas(ctx, session),
            Action::ChooseDatabases(parent) => self.choose_databases(ctx, parent),
            Action::SetTreeFilter(kind, scope, picked) => {
                self.set_tree_filter(ctx, kind, scope, picked);
            }
            Action::AdvancedCopy(tab_id) => self.open_copy_dialog(tab_id),
            Action::RememberCopy(format, headers) => self.copy_choice = (format, headers),
            Action::FollowKey(tab_id, row, column) => self.follow_key(tab_id, row, column),
            Action::FollowNestedKey(tab_id, depth, row, column) => {
                self.follow_nested_key(tab_id, depth, row, column);
            }
            Action::BackToReference(tab_id, depth) => self.back_to_reference(tab_id, depth),
            Action::CloseReference => self.reference = None,
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
            Action::AskAi(tab, mode) => self.open_ai(ctx, tab, mode),
            Action::AiGenerate => self.ai_generate(ctx),
            Action::AiInsert => self.ai_insert(ctx),
            Action::SaveAiSettings(settings, key) => self.save_ai_settings(ctx, settings, key),
            Action::ForgetAiKey(provider) => self.forget_ai_key(ctx, provider),
            Action::ToggleAgent => self.toggle_agent(ctx),
            Action::StartAgent => self.start_agent(ctx),
            Action::StopAgent => self.stop_agent(),
            Action::ClearAgent => self.clear_agent(ctx),
            Action::SendToAgent => self.send_to_agent(),
            Action::CancelAgent => self.cancel_agent(),
            Action::SetAgentCommand(command) => self.set_agent_command(ctx, command),
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
        let language = self
            .workspace
            .active
            .map_or(Language::Sql, |id| self.tab_language(id));
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
        if language == Language::Sql && !block.ends_with(';') {
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

    /// The saved connection behind `id`, which may be a session opened on
    /// another of its databases.
    fn profile_of(&self, id: ConnectionId) -> ConnectionId {
        self.explorer
            .opened
            .get(&id)
            .map_or(id, |(parent, _)| *parent)
    }

    /// F5, F2 and F4 on the tree's selected node, while the tree has the
    /// focus. Returns whether it has it.
    fn tree_shortcuts(&mut self, ctx: &egui::Context) -> bool {
        // With no tab open the tree is all there is.
        let focused = self.explorer.focused || self.workspace.tabs.is_empty();
        if !focused || ctx.memory(|memory| memory.focused().is_some()) {
            return false;
        }
        let Some(node) = self.explorer.selected.clone() else {
            return false;
        };
        let (refresh, rename, edit, delete) = ctx.input_mut(|input| {
            (
                shortcuts::consume(input, Command::Refresh),
                shortcuts::consume(input, Command::Rename),
                shortcuts::consume(input, Command::EditConnection),
                input.consume_key(egui::Modifiers::NONE, egui::Key::Delete),
            )
        });
        let id = match &node {
            NodeKey::ConnectionFolder(path) => {
                if rename {
                    self.actions
                        .push(Action::AskRename(rename_flow::RenameTarget::Folder(
                            path.clone(),
                        )));
                }
                return true;
            }
            NodeKey::Connection(id)
            | NodeKey::Database(id, _)
            | NodeKey::Scripts(id)
            | NodeKey::ScriptFolder(id, ..)
            | NodeKey::Schema(id, _)
            | NodeKey::Group(id, ..)
            | NodeKey::Object(id, ..)
            | NodeKey::Folder(id, _) => *id,
        };
        let redis = self.language(id) == Language::Redis;
        if refresh {
            let action = match &node {
                NodeKey::Connection(id) => {
                    self.explorer.link(*id).is_online().then_some(Action::Refresh(*id))
                }
                NodeKey::Database(parent, database) => self
                    .explorer
                    .opened(*parent, database)
                    .or_else(|| {
                        matches!(self.explorer.link(*parent), Link::Online(server) if server.database == *database)
                            .then_some(*parent)
                    })
                    .map(Action::Refresh),
                NodeKey::Schema(id, schema) | NodeKey::Object(id, schema, _) if redis => {
                    Some(Action::LoadKeys(*id, schema.clone(), false))
                }
                NodeKey::Schema(id, schema)
                | NodeKey::Group(id, schema, _)
                | NodeKey::Object(id, schema, _) => Some(Action::LoadObjects(*id, schema.clone())),
                NodeKey::Folder(id, state::Folder::Schemas) => Some(Action::Refresh(*id)),
                NodeKey::Folder(id, folder) => Some(Action::LoadFolder(*id, *folder)),
                NodeKey::Scripts(_) | NodeKey::ScriptFolder(..) | NodeKey::ConnectionFolder(_) => {
                    None
                }
            };
            self.actions.extend(action);
        }
        if rename {
            match &node {
                NodeKey::Object(id, schema, key) if redis => self.actions.push(Action::KeyDialog {
                    connection: *id,
                    schema: schema.clone(),
                    key: key.clone(),
                    op: key_flow::KeyOp::Rename,
                }),
                NodeKey::Object(id, schema, name) => {
                    self.actions
                        .push(Action::AskRename(rename_flow::RenameTarget::Relation {
                            connection: *id,
                            schema: schema.clone(),
                            name: name.clone(),
                        }));
                }
                NodeKey::Schema(id, name) if !redis => {
                    self.actions
                        .push(Action::AskRename(rename_flow::RenameTarget::Schema {
                            connection: *id,
                            name: name.clone(),
                        }));
                }
                NodeKey::Database(parent, name) => {
                    self.actions
                        .push(Action::AskRename(rename_flow::RenameTarget::Database {
                            connection: *parent,
                            name: name.clone(),
                        }));
                }
                NodeKey::Connection(id) => self.actions.push(Action::EditConnection(*id)),
                _ => {}
            }
        }
        if delete {
            match &node {
                NodeKey::Database(parent, name) => self
                    .actions
                    .push(Action::AskDropDatabase(*parent, name.clone())),
                NodeKey::Connection(id) => self.actions.push(Action::AskDeleteConnection(*id)),
                _ => {}
            }
        }
        if edit {
            let profile = match &node {
                NodeKey::Database(parent, _) => *parent,
                _ => self.profile_of(id),
            };
            self.actions.push(Action::EditConnection(profile));
        }
        true
    }

    /// What F5 reads again outside the tree: the active tab's rows, and
    /// the tree under the database it is on.
    fn refresh_here(&self) -> Vec<Action> {
        let Some(tab) = self.workspace.active() else {
            return Vec::new();
        };
        let mut actions = Vec::new();
        if tab.data().is_some() || tab.last_run.is_some() {
            actions.push(Action::RefreshTab(tab.id));
        }
        if let Some(connection) = tab.connection
            && self.explorer.link(connection).is_online()
        {
            actions.push(match tab.data() {
                Some(view) if self.language(connection) == Language::Redis => {
                    Action::LoadKeys(connection, view.schema.clone(), false)
                }
                Some(view) => Action::LoadObjects(connection, view.schema.clone()),
                None => Action::Refresh(connection),
            });
        }
        actions
    }

    /// Reads again the tabs whose auto-refresh is due, and wakes up for
    /// the next one.
    fn auto_refresh(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        let mut next: Option<Instant> = None;
        let mut due = Vec::new();
        for tab in &mut self.workspace.tabs {
            let Some((every, at)) = &mut tab.auto_refresh else {
                continue;
            };
            if *at <= now {
                *at = now + *every;
                let ready = tab.running.is_none()
                    && tab.edits.is_empty()
                    && tab
                        .connection
                        .is_some_and(|id| self.backend.is_connected(id));
                if ready {
                    due.push(tab.id);
                }
            }
            next = Some(next.map_or(*at, |next| next.min(*at)));
        }
        self.actions.extend(due.into_iter().map(Action::RefreshTab));
        if let Some(next) = next {
            ctx.request_repaint_after(next.saturating_duration_since(now));
        }
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        let modal_open = self.form.is_some()
            || self.prompt.is_some()
            || self.script_prompt.is_some()
            || self.cell_editor.is_some()
            || self.edit_review.is_some()
            || self.run_review.is_some()
            || self.confirm_delete.is_some()
            || self.confirm_project_delete.is_some()
            || self.close_confirm.is_some()
            || self.tx_confirm.is_some()
            || self.history_view.is_some()
            || self.task_dialog.is_some()
            || self.keys_dialog.is_some()
            || self.ai_dialog.is_some()
            || self.key_dialog.is_some()
            || self.rename_prompt.is_some()
            || self.drop_prompt.is_some();
        if modal_open {
            return;
        }
        let active = self.workspace.active;
        let tree = self.tree_shortcuts(ctx);
        ctx.input_mut(|input| {
            // The script shortcut first: it is the statement one plus Shift.
            if shortcuts::consume(input, Command::RunScript)
                && let Some(tab) = active
            {
                self.actions.push(Action::Run(tab, RunScope::Script));
            }
            if shortcuts::consume(input, Command::ExplainAnalyze)
                && let Some(tab) = active
            {
                self.actions
                    .push(Action::Run(tab, RunScope::ExplainAnalyze));
            }
            if shortcuts::consume(input, Command::Explain)
                && let Some(tab) = active
            {
                self.actions.push(Action::Run(tab, RunScope::Explain));
            }
            // Save as first: it is save plus Shift.
            if shortcuts::consume(input, Command::SaveAs)
                && let Some(tab) = active
            {
                self.actions.push(Action::SaveScriptAs(tab));
            }
            if shortcuts::consume(input, Command::Save)
                && let Some(tab) = active
            {
                self.actions.push(Action::SaveScript(tab));
            }
            if shortcuts::consume(input, Command::Open) {
                self.actions.push(Action::OpenScriptFile);
            }
            if shortcuts::consume(input, Command::History) {
                self.actions.push(Action::OpenHistory);
            }
            if shortcuts::consume(input, Command::RunStatement)
                && let Some(tab) = active
            {
                self.actions.push(Action::Run(tab, RunScope::Statement));
            }
            if shortcuts::consume(input, Command::Cancel)
                && let Some(tab) = active
            {
                self.actions.push(Action::Cancel(tab));
            }
            if shortcuts::consume(input, Command::NewEditor) {
                self.actions.push(Action::OpenEditor {
                    connection: None,
                    sql: String::new(),
                });
            }
            if !tree && shortcuts::consume(input, Command::Refresh) {
                self.actions.extend(self.refresh_here());
            }
            // F2 edits the grid's picked cell; otherwise it renames the tab.
            if !tree
                && let Some(tab) = self.workspace.active()
                && tab.grid_selection.focus.is_none()
                && shortcuts::consume(input, Command::Rename)
            {
                self.actions.push(Action::AskRename(rename_target(tab)));
            }
            if !tree
                && shortcuts::consume(input, Command::EditConnection)
                && let Some(id) = self.workspace.active().and_then(|tab| tab.connection)
            {
                self.actions
                    .push(Action::EditConnection(self.profile_of(id)));
            }
            if shortcuts::consume(input, Command::CloseTab)
                && let Some(tab) = active
            {
                self.actions.push(Action::CloseTab(tab));
            }
            if shortcuts::consume(input, Command::AskAi)
                && let Some(tab) = active
            {
                self.actions.push(Action::AskAi(tab, ai_flow::Mode::Write));
            }
            if shortcuts::consume(input, Command::ToggleAgent) {
                self.actions.push(Action::ToggleAgent);
            }
            if shortcuts::consume(input, Command::NewConnection) {
                self.actions.push(Action::NewConnection);
            }
            if shortcuts::consume(input, Command::ToggleSidebar) {
                self.actions.push(Action::ToggleSidebar);
            }
        });
    }

    fn central(&mut self, ui: &mut egui::Ui) {
        let fetch_size = self.settings.fetch_size;
        let palette = Palette::current(ui.ctx());
        if self.workspace.tabs.is_empty() {
            chrome::welcome(
                ui,
                self.profiles.all(),
                &self.explorer,
                &mut self.welcome_uri,
                &mut self.actions,
            );
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
        let language = self.tab_language(active);
        let editing = self.editing(active);
        let links = self.sync_reference(active);
        let profiles = &*connections(&self.profiles, &self.explorer);
        let explorer = &self.explorer;
        let keyword_case = self.settings.keyword_case;
        let completion_history = &self.completion_history;
        let actions = &mut self.actions;
        let reference = self.reference.as_ref().filter(|r| r.tab == active);
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
                chrome::toolbar(ui, tab, profiles, explorer, language, actions)
            });

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
                    results::show(
                        ui,
                        tab,
                        &editing,
                        &links.columns,
                        reference,
                        fetch_size,
                        actions,
                    )
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
                results::show(
                    ui,
                    tab,
                    &editing,
                    &links.columns,
                    reference,
                    fetch_size,
                    actions,
                );
            });
        self.layout.results = (total > 0.0).then(|| results.response.rect.height() / total);

        let family = tab
            .connection
            .and_then(|id| profiles.iter().find(|profile| profile.id == id))
            .map_or_else(Default::default, |profile| {
                crate::dialect::Family::of(profile.driver.as_str())
            });
        hints_flow::refresh(tab, explorer, language, family, actions);
        let search = tab
            .connection
            .map(|id| explorer.search_path(id, family))
            .unwrap_or_default();
        let quote = |name: &str| violet_core::Dialect::quote_ident(&dialect, name);
        let connected = match tab.connection.map(|id| explorer.link(id)) {
            Some(state::Link::Online(server)) => Some(server.database.clone()),
            _ => None,
        };
        // A key-value store's editor completes the keys of its database.
        let database = match tab.connection.map(|id| explorer.link(id)) {
            Some(state::Link::Online(server)) if language == Language::Redis => {
                Some(server.database.clone())
            }
            _ => None,
        };
        let found: Vec<(&str, Option<&[violet_core::DbObject]>)> = match (tab.connection, &database)
        {
            (Some(id), Some(database)) => explorer
                .key_matches
                .iter()
                .filter(|((owner, schema, _), _)| *owner == id && schema == database)
                .map(|((_, _, prefix), keys)| {
                    let keys = match keys {
                        Loadable::Ready(keys) => Some(&**keys),
                        _ => None,
                    };
                    (prefix.as_str(), keys)
                })
                .collect(),
            _ => Vec::new(),
        };
        let keys = database.as_ref().map(|database| {
            let tree = tab
                .connection
                .and_then(|id| explorer.keys.get(&(id, database.clone())))
                .filter(|keys| keys.listed);
            completion::Keyspace {
                schema: database.as_str(),
                listed: tree.map(|keys| &*keys.list),
                whole: tree.is_some_and(|keys| keys.next.is_none() && keys.applied == "*"),
                found: &found,
            }
        });
        let no_history = completion::History::default();
        let assist = editor::Assist {
            connection: tab.connection,
            language,
            catalog: &ConnectionCatalog {
                explorer,
                id: tab.connection,
            },
            options: completion::Options {
                search: &search,
                database: connected.as_deref(),
                quote: &quote,
                family,
                case: keyword_case,
                history: tab
                    .connection
                    .and_then(|id| completion_history.get(&id))
                    .unwrap_or(&no_history),
            },
            keys,
        };
        egui::CentralPanel::default()
            .frame(Frame::NONE.fill(palette.window))
            .show(ui, |ui| editor::show(ui, tab, &assist, actions));
    }

    fn modals(&mut self, ctx: &egui::Context) {
        if let Some(form) = &mut self.form {
            let drivers = self.providers.choices(self.backend.registry().infos());
            let folders = self.profiles.folders();
            if !connection_form::show(ctx, form, &drivers, &folders, &mut self.actions) {
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
        if let Some(dialog) = &mut self.key_dialog
            && !key_flow::show(ctx, dialog, &mut self.actions)
        {
            self.key_dialog = None;
        }
        if let Some(prompt) = &mut self.rename_prompt
            && !rename_flow::show(ctx, prompt, &mut self.actions)
        {
            self.rename_prompt = None;
        }
        if let Some(prompt) = &mut self.drop_prompt
            && !rename_flow::show_drop(ctx, prompt, &mut self.actions)
        {
            self.drop_prompt = None;
        }
        if let Some(chooser) = &mut self.chooser
            && !catalog_flow::show_chooser(ctx, chooser, &mut self.actions)
        {
            self.chooser = None;
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
            let redis = connections(&self.profiles, &self.explorer)
                .iter()
                .filter(|profile| {
                    self.explorer.language(profile.driver.as_str()) == Language::Redis
                })
                .map(|profile| profile.id)
                .collect();
            let target = history::Target {
                editor: active.is_some_and(|tab| tab.data().is_none()),
                connection: active.and_then(|tab| tab.connection),
                redis,
            };
            if !history::show(ctx, view, &self.history, &target, &mut self.actions) {
                self.history_view = None;
            }
        }
        if let Some(view) = &self.table_stats
            && !stats_flow::show(ctx, view, &mut self.actions)
        {
            self.table_stats = None;
        }
        if let Some(view) = &mut self.health
            && !health_flow::show(ctx, view, &mut self.actions)
        {
            self.health = None;
        }
        self.task_modal(ctx);
        if let Some(dialog) = &mut self.keys_dialog
            && !keys_flow::show(ctx, dialog, &mut self.actions)
        {
            self.keys_dialog = None;
        }
        self.ai_modal(ctx);
        if let Some(answer) = self
            .close_confirm
            .as_ref()
            .and_then(|confirm| script_flow::confirm_close(ctx, confirm))
            && let Some(confirm) = self.close_confirm.take()
        {
            self.answer_close(ctx, confirm, answer);
        }
        if let Some(answer) = self
            .tx_confirm
            .as_ref()
            .and_then(|confirm| transaction_flow::confirm_transactions(ctx, confirm))
            && let Some(confirm) = self.tx_confirm.take()
        {
            self.answer_transactions(ctx, confirm, answer);
        }
        if let Some(id) = self.confirm_project_delete.clone() {
            let name = self
                .settings
                .projects
                .iter()
                .find(|project| project.id == id)
                .map_or_else(String::new, |project| project.name.clone());
            match connection_form::confirm(
                ctx,
                "Delete workspace?",
                &format!(
                    "\u{201C}{name}\u{201D}, its connections, their saved passwords and its \
                     scripts will be removed. The databases themselves are not touched."
                ),
            ) {
                Some(true) => {
                    self.actions.push(Action::DeleteProject(id));
                    self.confirm_project_delete = None;
                }
                Some(false) => self.confirm_project_delete = None,
                None => {}
            }
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
    #[cfg(debug_assertions)]
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        if let Some(snapshot) = &mut self.snapshot {
            snapshot.inject(raw_input);
        }
    }

    #[hotpath::measure]
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.explorer.languages = self
            .backend
            .registry()
            .infos()
            .into_iter()
            .map(|info| (info.id.as_str().to_owned(), info.language))
            .collect();
        self.handle_events(ctx);
        self.retry_loads(ctx);
        self.poll_agent(ctx);
        self.capture_shortcut(ctx);
        self.shortcuts(ctx);
        self.auto_refresh(ctx);
        self.health_tick(ctx);
        self.ping_tick(ctx);
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
        let closing = closing && !self.keeps_window(ctx) && !self.keeps_transactions(ctx);
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
                if self
                    .updates
                    .as_ref()
                    .is_some_and(crate::update::Updates::should_quit)
                {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                let update = self
                    .updates
                    .as_ref()
                    .map(crate::update::Updates::status)
                    .unwrap_or_default();
                chrome::title_bar(
                    ui,
                    &title,
                    &self.settings,
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
                    &self.profiles.folders(),
                    &self.script_list,
                    &self.script_folders,
                    &mut self.explorer,
                    &mut self.actions,
                );
            });
        // A click anywhere else takes the F-key shortcuts from the tree.
        if let Some(sidebar) = &sidebar
            && ctx.input(|input| {
                input.pointer.any_pressed()
                    && input
                        .pointer
                        .interact_pos()
                        .is_some_and(|pos| !sidebar.response.rect.contains(pos))
            })
        {
            self.explorer.focused = false;
        }
        if std::mem::take(&mut self.explorer.picked) {
            self.follow_tree(&ctx);
        }
        self.layout.sidebar = sidebar.map(|sidebar| sidebar.response.rect.width());
        if sidebar_open != self.settings.sidebar {
            self.actions.push(Action::ToggleSidebar);
        }

        if self.agent.open {
            let command = if self.settings.agent_command.trim().is_empty() {
                crate::agent::DEFAULT_COMMAND
            } else {
                self.settings.agent_command.as_str()
            };
            Panel::right("agent")
                .resizable(true)
                .default_size(380.0)
                .size_range(300.0..=720.0)
                .frame(
                    Frame::NONE
                        .fill(palette.panel)
                        .stroke(Stroke::new(1.0, palette.outline)),
                )
                .show(ui, |ui| {
                    ui.set_min_height(ui.available_height());
                    agent_flow::show(ui, &mut self.agent, command, &mut self.actions);
                });
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

/// What F2 renames for a tab: a data tab's relation, else the tab.
fn rename_target(tab: &QueryTab) -> rename_flow::RenameTarget {
    match (tab.connection, tab.data()) {
        (Some(connection), Some(view)) => rename_flow::RenameTarget::Relation {
            connection,
            schema: view.schema.clone(),
            name: view.relation.clone(),
        },
        _ => rename_flow::RenameTarget::Tab(tab.id),
    }
}
