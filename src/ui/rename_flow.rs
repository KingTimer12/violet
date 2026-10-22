//! Renaming with F2: a tab, the script file behind it, or a table or view.

use egui::{CornerRadius, RichText, Stroke};
use violet_app::{CachePolicy, ExecOptions};
use violet_core::{ConnectionId, ObjectKind, QueryOutput, Result};

use super::VioletApp;
use super::action::Action;
use super::definition_flow::Family;
use super::icons::Icon;
use super::state::{Link, Loadable};
use super::theme::{self, Palette};
use super::toasts::Toast;
use super::widgets::{self, Tone};
use super::workspace::TabId;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RenameTarget {
    Tab(TabId),
    Schema {
        connection: ConnectionId,
        name: String,
    },
    /// A database of a connection that lists them all.
    Database {
        connection: ConnectionId,
        name: String,
    },
    Relation {
        connection: ConnectionId,
        schema: String,
        name: String,
    },
    /// Not a rename: the name of a database to create.
    NewDatabase(ConnectionId),
    /// A new connection folder, inside this one (empty for the top level).
    NewFolder(String),
    /// A connection folder, by its path.
    Folder(String),
    /// A new folder of scripts, inside `parent` of a database.
    NewScriptFolder {
        connection: ConnectionId,
        database: String,
        parent: String,
    },
}

pub struct RenamePrompt {
    target: RenameTarget,
    text: String,
    error: Option<String>,
}

/// A rename sent to the server, kept until it answers.
pub struct Pending {
    connection: ConnectionId,
    schema: String,
    from: String,
    to: String,
    what: Renamed,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Renamed {
    Relation,
    Schema,
    Database,
    Created,
    Dropped,
}

/// Deleting a database: typed out in full, name and a random number, so
/// it never happens by a slip.
pub struct DropPrompt {
    connection: ConnectionId,
    name: String,
    phrase: String,
    text: String,
    copied: Option<std::time::Instant>,
}

impl VioletApp {
    pub(super) fn ask_rename(&mut self, ctx: &egui::Context, target: RenameTarget) {
        let text = match &target {
            RenameTarget::NewFolder(_) | RenameTarget::NewScriptFolder { .. } => String::new(),
            RenameTarget::Folder(path) => path.rsplit('/').next().unwrap_or(path).to_owned(),
            RenameTarget::Tab(id) => match self.workspace.get(*id) {
                Some(tab) => tab.title.clone(),
                None => return,
            },
            RenameTarget::Schema { connection, name } => {
                if self.family(*connection) != Family::Postgres {
                    self.toasts.push(
                        ctx,
                        Toast::info("Violet renames schemas on PostgreSQL only"),
                    );
                    return;
                }
                name.clone()
            }
            RenameTarget::Database { connection, name } => {
                if self.family(*connection) != Family::Postgres {
                    self.toasts.push(
                        ctx,
                        Toast::info("Violet renames databases on PostgreSQL only"),
                    );
                    return;
                }
                if matches!(self.explorer.link(*connection), Link::Online(server) if server.database == *name)
                {
                    self.toasts.push(
                        ctx,
                        Toast::error(
                            "Cannot rename the database Violet is connected to",
                            "Point the connection at another database, then rename this one.",
                        ),
                    );
                    return;
                }
                name.clone()
            }
            RenameTarget::Relation {
                connection,
                schema,
                name,
            } => {
                if self.language(*connection) == violet_core::Language::Redis {
                    self.open_key_dialog(
                        *connection,
                        schema.clone(),
                        name.clone(),
                        super::key_flow::KeyOp::Rename,
                    );
                    return;
                }
                if self.rename_sql(*connection, schema, name, name).is_none() {
                    self.toasts.push(
                        ctx,
                        Toast::info("Violet cannot rename this object on this database"),
                    );
                    return;
                }
                name.clone()
            }
            RenameTarget::NewDatabase(connection) => {
                if !matches!(self.family(*connection), Family::Postgres | Family::MySql) {
                    self.toasts.push(
                        ctx,
                        Toast::info("Violet creates databases on PostgreSQL and MySQL only"),
                    );
                    return;
                }
                String::new()
            }
        };
        self.rename_prompt = Some(RenamePrompt {
            target,
            text,
            error: None,
        });
    }

    pub(super) fn rename(&mut self, ctx: &egui::Context, to: String) {
        let Some(prompt) = &mut self.rename_prompt else {
            return;
        };
        let to = to.trim().to_owned();
        if to.is_empty() {
            prompt.error = Some("Type a name.".into());
            return;
        }
        match prompt.target.clone() {
            RenameTarget::NewFolder(_)
            | RenameTarget::Folder(_)
            | RenameTarget::NewScriptFolder { .. }
                if to.contains(['/', '\\']) =>
            {
                prompt.error = Some("A folder name cannot hold / or \\.".into());
                return;
            }
            RenameTarget::NewScriptFolder {
                connection,
                database,
                parent,
            } => {
                let folder = folder_path(&parent, &to);
                if self
                    .scripts
                    .folder_path(connection, &database, &folder)
                    .exists()
                {
                    prompt.error = Some("There is a folder with this name here.".into());
                    return;
                }
                if let Err(error) = self.create_script_folder(ctx, connection, &database, &folder) {
                    if let Some(prompt) = &mut self.rename_prompt {
                        prompt.error = Some(error.to_string());
                    }
                    return;
                }
                let key = if parent.is_empty() {
                    super::state::NodeKey::Scripts(connection)
                } else {
                    super::state::NodeKey::ScriptFolder(connection, database, parent)
                };
                self.explorer.expanded.insert(key);
            }
            RenameTarget::NewFolder(parent) => {
                let path = folder_path(&parent, &to);
                if self.profiles.folders().contains(&path) {
                    prompt.error = Some("There is a folder with this name here.".into());
                    return;
                }
                if let Err(error) = self.profiles.add_folder(&path) {
                    prompt.error = Some(error.to_string());
                    return;
                }
                self.explorer
                    .expanded
                    .insert(super::state::NodeKey::ConnectionFolder(parent));
            }
            RenameTarget::Folder(from) => {
                let path = folder_path(violet_app::parent_folder(&from), &to);
                if path != from {
                    if self.profiles.folders().contains(&path) {
                        prompt.error = Some("There is a folder with this name here.".into());
                        return;
                    }
                    if let Err(error) = self.profiles.rename_folder(&from, &path) {
                        prompt.error = Some(error.to_string());
                        return;
                    }
                }
            }
            RenameTarget::Tab(id) => {
                if let Err(error) = self.rename_tab(ctx, id, &to) {
                    if let Some(prompt) = &mut self.rename_prompt {
                        prompt.error = Some(error);
                    }
                    return;
                }
            }
            RenameTarget::Relation {
                connection,
                schema,
                name,
            } => {
                if to == name {
                    self.rename_prompt = None;
                    return;
                }
                if !self.backend.is_connected(connection) {
                    prompt.error = Some("Connect first.".into());
                    return;
                }
                let Some(sql) = self.rename_sql(connection, &schema, &name, &to) else {
                    return;
                };
                let request = self.backend.execute_in(
                    connection,
                    None,
                    sql,
                    ExecOptions {
                        cache: CachePolicy::Off,
                        page: None,
                    },
                );
                self.rename_requests.insert(
                    request,
                    Pending {
                        connection,
                        schema,
                        from: name,
                        to,
                        what: Renamed::Relation,
                    },
                );
            }
            RenameTarget::Schema { connection, name } => {
                if to == name {
                    self.rename_prompt = None;
                    return;
                }
                if !self.backend.is_connected(connection) {
                    prompt.error = Some("Connect first.".into());
                    return;
                }
                let double = |ident: &str| format!("\"{}\"", ident.replace('"', "\"\""));
                let request = self.backend.execute_in(
                    connection,
                    None,
                    format!("ALTER SCHEMA {} RENAME TO {}", double(&name), double(&to)),
                    ExecOptions {
                        cache: CachePolicy::Off,
                        page: None,
                    },
                );
                self.rename_requests.insert(
                    request,
                    Pending {
                        connection,
                        schema: String::new(),
                        from: name,
                        to,
                        what: Renamed::Schema,
                    },
                );
            }
            RenameTarget::Database { connection, name } => {
                if to == name {
                    self.rename_prompt = None;
                    return;
                }
                if !self.backend.is_connected(connection) {
                    prompt.error = Some("Connect first.".into());
                    return;
                }
                // PostgreSQL refuses while a session is on it, Violet's too.
                if let Some(session) = self.explorer.opened(connection, &name) {
                    self.actions.push(Action::Disconnect(session));
                }
                let double = |ident: &str| format!("\"{}\"", ident.replace('"', "\"\""));
                let request = self.backend.execute_in(
                    connection,
                    None,
                    format!("ALTER DATABASE {} RENAME TO {}", double(&name), double(&to)),
                    ExecOptions {
                        cache: CachePolicy::Off,
                        page: None,
                    },
                );
                self.rename_requests.insert(
                    request,
                    Pending {
                        connection,
                        schema: String::new(),
                        from: name,
                        to,
                        what: Renamed::Database,
                    },
                );
            }
            RenameTarget::NewDatabase(connection) => {
                if !self.backend.is_connected(connection) {
                    prompt.error = Some("Connect first.".into());
                    return;
                }
                let name = match self.family(connection) {
                    Family::MySql => format!("`{}`", to.replace('`', "``")),
                    _ => format!("\"{}\"", to.replace('"', "\"\"")),
                };
                let request = self.backend.execute_in(
                    connection,
                    None,
                    format!("CREATE DATABASE {name}"),
                    ExecOptions {
                        cache: CachePolicy::Off,
                        page: None,
                    },
                );
                self.rename_requests.insert(
                    request,
                    Pending {
                        connection,
                        schema: String::new(),
                        from: String::new(),
                        to,
                        what: Renamed::Created,
                    },
                );
            }
        }
        self.rename_prompt = None;
    }

    pub(super) fn ask_drop_database(
        &mut self,
        ctx: &egui::Context,
        connection: ConnectionId,
        name: String,
    ) {
        if !matches!(self.family(connection), Family::Postgres | Family::MySql) {
            self.toasts.push(
                ctx,
                Toast::info("Violet deletes databases on PostgreSQL and MySQL only"),
            );
            return;
        }
        if matches!(self.explorer.link(connection), Link::Online(server) if server.database == name)
        {
            self.toasts.push(
                ctx,
                Toast::error(
                    "Cannot delete the database Violet is connected to",
                    "Point the connection at another database, then delete this one.",
                ),
            );
            return;
        }
        let number = std::hash::BuildHasher::hash_one(
            &std::collections::hash_map::RandomState::new(),
            &name,
        ) % 900_000
            + 100_000;
        self.drop_prompt = Some(DropPrompt {
            phrase: format!("{name}-{number}"),
            connection,
            name,
            text: String::new(),
            copied: None,
        });
    }

    pub(super) fn drop_database(&mut self, ctx: &egui::Context) {
        let Some(prompt) = self.drop_prompt.take() else {
            return;
        };
        if prompt.text != prompt.phrase {
            self.drop_prompt = Some(prompt);
            return;
        }
        let DropPrompt {
            connection, name, ..
        } = prompt;
        if !self.backend.is_connected(connection) {
            self.toasts
                .push(ctx, Toast::error("Not connected", "Connect first."));
            return;
        }
        // PostgreSQL refuses while a session is on it, Violet's too.
        if let Some(session) = self.explorer.opened(connection, &name) {
            self.actions.push(Action::Disconnect(session));
        }
        let quoted = match self.family(connection) {
            Family::MySql => format!("`{}`", name.replace('`', "``")),
            _ => format!("\"{}\"", name.replace('"', "\"\"")),
        };
        let request = self.backend.execute_in(
            connection,
            None,
            format!("DROP DATABASE {quoted}"),
            ExecOptions {
                cache: CachePolicy::Off,
                page: None,
            },
        );
        self.rename_requests.insert(
            request,
            Pending {
                connection,
                schema: String::new(),
                from: name,
                to: String::new(),
                what: Renamed::Dropped,
            },
        );
    }

    /// Retitles a tab; a saved script's file is renamed with it.
    fn rename_tab(
        &mut self,
        ctx: &egui::Context,
        id: TabId,
        to: &str,
    ) -> std::result::Result<(), String> {
        let Some(tab) = self.workspace.get_mut(id) else {
            return Ok(());
        };
        let Some(file) = &mut tab.file else {
            tab.title = to.to_owned();
            return Ok(());
        };
        if to.contains(['/', '\\']) {
            return Err("A name cannot hold / or \\.".into());
        }
        let mut name = to.to_owned();
        if let Some(extension) = file.path.extension() {
            name = format!("{name}.{}", extension.to_string_lossy());
        }
        let path = file.path.with_file_name(name);
        if path == file.path {
            return Ok(());
        }
        if path.exists() {
            return Err("A script with this name is already saved here.".into());
        }
        if file.path.exists() {
            std::fs::rename(&file.path, &path).map_err(|error| error.to_string())?;
        }
        tab.title = super::script_flow::script_title(&self.scripts, &path);
        file.path = path;
        self.reload_scripts(ctx);
        Ok(())
    }

    /// The statement that renames `schema.name` to `to`, when this
    /// database and kind of object have one.
    fn rename_sql(
        &self,
        connection: ConnectionId,
        schema: &str,
        name: &str,
        to: &str,
    ) -> Option<String> {
        let kind = match self.explorer.objects.get(&(connection, schema.to_owned())) {
            Some(Loadable::Ready(objects)) => objects
                .iter()
                .find(|object| object.name == name)
                .map(|object| object.kind),
            _ => None,
        }
        .unwrap_or(ObjectKind::Table);
        let double = |ident: &str| format!("\"{}\"", ident.replace('"', "\"\""));
        let tick = |ident: &str| format!("`{}`", ident.replace('`', "``"));
        match self.family(connection) {
            Family::Postgres => {
                let what = match kind {
                    ObjectKind::Table => "TABLE",
                    ObjectKind::View => "VIEW",
                    ObjectKind::MaterializedView => "MATERIALIZED VIEW",
                    ObjectKind::ForeignTable => "FOREIGN TABLE",
                    ObjectKind::Sequence => "SEQUENCE",
                    ObjectKind::Function | ObjectKind::Key => return None,
                };
                Some(format!(
                    "ALTER {what} {}.{} RENAME TO {}",
                    double(schema),
                    double(name),
                    double(to)
                ))
            }
            Family::MySql => matches!(kind, ObjectKind::Table | ObjectKind::View).then(|| {
                format!(
                    "RENAME TABLE {}.{} TO {}.{}",
                    tick(schema),
                    tick(name),
                    tick(schema),
                    tick(to)
                )
            }),
            Family::Sqlite => matches!(kind, ObjectKind::Table).then(|| {
                format!(
                    "ALTER TABLE {}.{} RENAME TO {}",
                    double(schema),
                    double(name),
                    double(to)
                )
            }),
            Family::Other => None,
        }
    }

    pub(super) fn renamed(
        &mut self,
        ctx: &egui::Context,
        pending: Pending,
        result: &Result<QueryOutput>,
    ) {
        if let Err(error) = result {
            let title = match pending.what {
                Renamed::Created => format!("Could not create {}", pending.to),
                Renamed::Dropped => format!("Could not delete {}", pending.from),
                _ => format!("Could not rename {}", pending.from),
            };
            self.toasts
                .push(ctx, Toast::error(title, error.message.clone()));
            return;
        }
        if pending.what == Renamed::Schema {
            for tab in &mut self.workspace.tabs {
                if tab.connection == Some(pending.connection)
                    && let Some(view) = tab.data_mut()
                    && view.schema == pending.from
                {
                    view.schema = pending.to.clone();
                }
            }
        }
        if pending.what == Renamed::Dropped {
            self.toasts
                .push(ctx, Toast::success(format!("Deleted {}", pending.from)));
            self.actions.push(Action::Refresh(pending.connection));
            return;
        }
        if pending.what == Renamed::Created {
            self.toasts
                .push(ctx, Toast::success(format!("Created {}", pending.to)));
            self.actions.push(Action::Refresh(pending.connection));
            return;
        }
        if pending.what != Renamed::Relation {
            self.toasts.push(
                ctx,
                Toast::success(format!("Renamed {} to {}", pending.from, pending.to)),
            );
            self.actions.push(Action::Refresh(pending.connection));
            return;
        }
        for tab in &mut self.workspace.tabs {
            if tab.connection != Some(pending.connection) {
                continue;
            }
            let Some(view) = tab.data_mut() else {
                continue;
            };
            if view.schema == pending.schema && view.relation == pending.from {
                view.relation = pending.to.clone();
                if tab.title == pending.from {
                    tab.title = pending.to.clone();
                }
            }
        }
        self.toasts.push(
            ctx,
            Toast::success(format!("Renamed {} to {}", pending.from, pending.to)),
        );
        self.load_objects(pending.connection, pending.schema);
    }
}

fn folder_path(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_owned()
    } else {
        format!("{parent}/{name}")
    }
}

/// Asks for the new name. Returns whether it is still open.
pub fn show(ctx: &egui::Context, prompt: &mut RenamePrompt, actions: &mut Vec<Action>) -> bool {
    let palette = Palette::current(ctx);
    let mut open = true;
    let (title, body) = match &prompt.target {
        RenameTarget::NewFolder(_) => ("New folder", "Folders group connections in the tree."),
        RenameTarget::NewScriptFolder { .. } => (
            "New folder",
            "A folder on disk, below the database's scripts.",
        ),

        RenameTarget::Folder(_) => (
            "Rename folder",
            "The connections and folders inside move with it.",
        ),
        RenameTarget::Tab(_) => ("Rename tab", "A saved script's file is renamed with it."),
        RenameTarget::Schema { .. } => (
            "Rename schema",
            "Renamed on the server. Queries that name the old schema stop working.",
        ),
        RenameTarget::Database { .. } => (
            "Rename database",
            "Its open sessions are closed first. PostgreSQL refuses while others use it.",
        ),
        RenameTarget::Relation { .. } => (
            "Rename",
            "Renamed on the server. Queries and views that use the old name stop working.",
        ),
        RenameTarget::NewDatabase(_) => (
            "New database",
            "Created on the server with its default settings.",
        ),
    };
    let (icon, verb) = match &prompt.target {
        RenameTarget::NewDatabase(_)
        | RenameTarget::NewFolder(_)
        | RenameTarget::NewScriptFolder { .. } => (Icon::Plus, "Create"),
        _ => (Icon::Pencil, "Rename"),
    };
    let modal = egui::Modal::new(egui::Id::new("rename-prompt"))
        .frame(
            egui::Frame::new()
                .fill(palette.surface)
                .stroke(Stroke::new(1.0, palette.outline))
                .corner_radius(CornerRadius::same(14))
                .inner_margin(egui::Margin::same(22)),
        )
        .show(ctx, |ui| {
            ui.set_width(380.0);
            ui.spacing_mut().item_spacing.y = 12.0;
            ui.label(
                RichText::new(title)
                    .font(theme::semibold(16.0))
                    .color(palette.text),
            );
            ui.label(RichText::new(body).color(palette.secondary));
            let response = ui.add(widgets::text_input(&mut prompt.text, "Name"));
            response.request_focus();
            // The field asks for focus each frame, so it never reports
            // losing it to Enter.
            let submit = ui.input(|input| input.key_pressed(egui::Key::Enter));
            if let Some(error) = &prompt.error {
                ui.label(RichText::new(error).color(palette.danger));
            }
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if widgets::button(ui, Tone::Primary, Some(icon), verb).clicked() || submit {
                        actions.push(Action::RenameTo(prompt.text.clone()));
                    }
                    if widgets::button(ui, Tone::Ghost, None, "Cancel").clicked() {
                        open = false;
                    }
                });
            });
        });
    open && !modal.should_close()
}

/// Asks to type the confirmation phrase. Returns whether it is still open.
pub fn show_drop(ctx: &egui::Context, prompt: &mut DropPrompt, actions: &mut Vec<Action>) -> bool {
    let palette = Palette::current(ctx);
    let mut open = true;
    let modal = egui::Modal::new(egui::Id::new("drop-database"))
        .frame(
            egui::Frame::new()
                .fill(palette.surface)
                .stroke(Stroke::new(1.0, palette.outline))
                .corner_radius(CornerRadius::same(14))
                .inner_margin(egui::Margin::same(22)),
        )
        .show(ctx, |ui| {
            ui.set_width(400.0);
            ui.spacing_mut().item_spacing.y = 12.0;
            ui.label(
                RichText::new(format!("Delete database {}", prompt.name))
                    .font(theme::semibold(16.0))
                    .color(palette.danger),
            );
            ui.label(
                RichText::new(
                    "Every table and row in it is gone for good. Its open sessions are closed first.",
                )
                .color(palette.secondary),
            );
            ui.label(RichText::new("Type this to confirm:").color(palette.secondary));
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(&prompt.phrase)
                        .font(theme::mono(13.5))
                        .color(palette.text),
                );
                let copied = prompt
                    .copied
                    .is_some_and(|at| at.elapsed() < std::time::Duration::from_millis(1500));
                if copied {
                    ui.ctx().request_repaint();
                }
                let (icon, label) = if copied {
                    (Icon::CircleCheck, "Copied")
                } else {
                    (Icon::Copy, "Copy")
                };
                if widgets::button(ui, Tone::Ghost, Some(icon), label).clicked() {
                    prompt.copied = Some(std::time::Instant::now());
                    actions.push(Action::Copy(prompt.phrase.clone()));
                }
            });
            let response = ui.add(widgets::text_input(&mut prompt.text, &prompt.phrase));
            response.request_focus();
            let matches = prompt.text == prompt.phrase;
            let submit = matches && ui.input(|input| input.key_pressed(egui::Key::Enter));
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add_enabled_ui(matches, |ui| {
                            widgets::button(ui, Tone::Danger, Some(Icon::Trash), "Delete")
                        })
                        .inner
                        .clicked()
                        || submit
                    {
                        actions.push(Action::DropDatabase);
                    }
                    if widgets::button(ui, Tone::Ghost, None, "Cancel").clicked() {
                        open = false;
                    }
                });
            });
        });
    open && !modal.should_close()
}
