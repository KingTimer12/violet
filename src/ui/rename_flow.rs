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
}

impl VioletApp {
    pub(super) fn ask_rename(&mut self, ctx: &egui::Context, target: RenameTarget) {
        let text = match &target {
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
        }
        self.rename_prompt = None;
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
            self.toasts.push(
                ctx,
                Toast::error(
                    format!("Could not rename {}", pending.from),
                    error.message.clone(),
                ),
            );
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

/// Asks for the new name. Returns whether it is still open.
pub fn show(ctx: &egui::Context, prompt: &mut RenamePrompt, actions: &mut Vec<Action>) -> bool {
    let palette = Palette::current(ctx);
    let mut open = true;
    let (title, body) = match &prompt.target {
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
            let submit =
                response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
            if let Some(error) = &prompt.error {
                ui.label(RichText::new(error).color(palette.danger));
            }
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if widgets::button(ui, Tone::Primary, Some(Icon::Pencil), "Rename").clicked()
                        || submit
                    {
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
