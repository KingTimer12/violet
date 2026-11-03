//! Saving editors as scripts and opening them again.
//!
//! A script saved with ⌘/Ctrl + S belongs to its editor's connection and
//! database: it goes to Violet's scripts folder, under them, and the tree
//! lists it under the connection. Opening it puts the editor back on that
//! database. "Save as" and "Open" take `.sql` files from anywhere.

use std::path::{Path, PathBuf};

use egui::{CornerRadius, RichText, Stroke};
use violet_core::ConnectionId;
use violet_storage::scripts;

use super::VioletApp;
use super::action::Action;
use super::icons::Icon;
use super::state::{Link, NodeKey};
use super::theme::{self, Palette};
use super::toasts::Toast;
use super::widgets::{self, Tone};
use super::workspace::{QueryTab, ScriptFile, TabId, TabKind};

/// Asks what to call a script saved for the first time.
pub struct ScriptPrompt {
    pub tab: TabId,
    pub name: String,
    /// Where it goes: the connection and database.
    pub target: String,
    pub error: Option<String>,
    /// Kept with the version this save makes.
    pub comment: String,
    /// Whether the script still needs a name; else it is saved to its
    /// file and only the comment is asked.
    pub ask_name: bool,
}

/// Closing editors whose scripts have changes not saved to their files.
pub struct CloseConfirm {
    /// Every tab being closed, saved or not.
    tabs: Vec<TabId>,
    /// The titles of the unsaved ones.
    names: Vec<String>,
    /// Whether the window closes, rather than the tabs.
    quit: bool,
}

/// What to do with the unsaved changes of the tabs being closed.
#[derive(Clone, Copy)]
pub enum CloseAnswer {
    Save,
    Discard,
    Cancel,
}

impl VioletApp {
    /// Closes `ids`, asking first when a script among them has unsaved
    /// changes.
    pub(super) fn close_tabs(&mut self, ids: Vec<TabId>) {
        let names: Vec<String> = ids
            .iter()
            .filter_map(|id| self.workspace.get(*id))
            .filter(|tab| tab.unsaved())
            .map(|tab| tab.title.clone())
            .collect();
        if names.is_empty() {
            for id in ids {
                self.close_tab(id);
            }
        } else {
            self.close_confirm = Some(CloseConfirm {
                tabs: ids,
                names,
                quit: false,
            });
        }
    }

    /// Keeps the window open when it is asked to close with unsaved
    /// scripts, and asks about them. Closing to install an update asks
    /// nothing: the session keeps the editors' text.
    pub(super) fn keeps_window(&mut self, ctx: &egui::Context) -> bool {
        if self.quitting
            || self
                .updates
                .as_ref()
                .is_some_and(crate::update::Updates::should_quit)
        {
            return false;
        }
        let unsaved: Vec<&QueryTab> = self
            .workspace
            .tabs
            .iter()
            .filter(|tab| tab.unsaved())
            .collect();
        if unsaved.is_empty() {
            return false;
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        if self.close_confirm.is_none() {
            self.close_confirm = Some(CloseConfirm {
                tabs: unsaved.iter().map(|tab| tab.id).collect(),
                names: unsaved.iter().map(|tab| tab.title.clone()).collect(),
                quit: true,
            });
        }
        true
    }

    /// Closes the window once its scripts are dealt with.
    pub(super) fn quit(&mut self, ctx: &egui::Context) {
        self.quitting = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    /// Closes a tab, stopping what it runs.
    fn close_tab(&mut self, id: TabId) {
        if let Some(tab) = self.workspace.close(id)
            && let Some(running) = tab.running
        {
            self.backend.cancel(running.request);
        }
    }

    pub(super) fn answer_close(
        &mut self,
        ctx: &egui::Context,
        confirm: CloseConfirm,
        answer: CloseAnswer,
    ) {
        if confirm.quit {
            match answer {
                CloseAnswer::Cancel => {}
                // The editors reopen next time as their files are.
                CloseAnswer::Discard => {
                    for tab in &mut self.workspace.tabs {
                        if let Some(file) = &tab.file {
                            tab.sql.clone_from(&file.saved);
                        }
                    }
                    self.quit(ctx);
                }
                CloseAnswer::Save => {
                    for id in confirm.tabs {
                        self.save_script(ctx, id);
                    }
                    // A script that could not be saved keeps the window.
                    if !self.workspace.tabs.iter().any(QueryTab::unsaved) {
                        self.quit(ctx);
                    }
                }
            }
            return;
        }
        match answer {
            CloseAnswer::Cancel => {}
            CloseAnswer::Discard => {
                for id in confirm.tabs {
                    self.close_tab(id);
                }
            }
            CloseAnswer::Save => {
                for id in confirm.tabs {
                    if self.workspace.get(id).is_some_and(QueryTab::unsaved) {
                        self.save_script(ctx, id);
                    }
                    // One that could not be saved stays open.
                    if !self.workspace.get(id).is_some_and(QueryTab::unsaved) {
                        self.close_tab(id);
                    }
                }
            }
        }
    }

    /// Lists the saved scripts again.
    pub(super) fn reload_scripts(&mut self, ctx: &egui::Context) {
        match self.scripts.list() {
            Ok((list, folders)) => {
                self.script_list = list;
                self.script_folders = folders;
            }
            Err(error) => self.toasts.push(
                ctx,
                Toast::error("The saved scripts could not be listed", error.to_string()),
            ),
        }
    }

    /// The saved connection and database a session runs on, which its
    /// scripts are saved for.
    fn script_target(&self, session: ConnectionId) -> Option<(ConnectionId, String)> {
        if let Some((parent, database)) = self.explorer.opened.get(&session) {
            return Some((*parent, database.clone()));
        }
        let profile = self.profiles.get(session)?;
        let database = match self.explorer.link(session) {
            Link::Online(server) if !server.database.is_empty() => server.database.clone(),
            _ => profile.database.clone(),
        };
        (!database.is_empty()).then_some((session, database))
    }

    /// The session for `database` of saved connection `parent`: the
    /// connection's own when it is its database, else one of its own,
    /// made the first time it is asked for.
    pub(super) fn database_session(
        &mut self,
        parent: ConnectionId,
        database: &str,
    ) -> Option<ConnectionId> {
        let profile = self.profiles.get(parent)?;
        let own = match self.explorer.link(parent) {
            Link::Online(server) => server.database == database,
            _ => profile.database == database,
        };
        if own {
            return Some(parent);
        }
        Some(match self.explorer.opened(parent, database) {
            Some(id) => id,
            None => {
                let id = ConnectionId::new();
                self.explorer
                    .opened
                    .insert(id, (parent, database.to_owned()));
                id
            }
        })
    }

    pub(super) fn script_action(&mut self, ctx: &egui::Context, action: Action) {
        match action {
            Action::SaveScript(tab) => self.save_script(ctx, tab),
            Action::SaveScriptAs(tab) => self.save_script_as(ctx, tab),
            Action::SaveScriptNamed(tab, name, comment) => {
                self.pending_comment = Some(comment);
                self.save_script_named(ctx, tab, &name);
                self.pending_comment = None;
            }
            Action::CommentSave(tab) => {
                let saved = self
                    .workspace
                    .get(tab)
                    .is_some_and(|tab| tab.file.is_some());
                if saved {
                    self.script_prompt = Some(ScriptPrompt {
                        tab,
                        name: String::new(),
                        target: String::new(),
                        error: None,
                        comment: String::new(),
                        ask_name: false,
                    });
                } else {
                    self.save_script(ctx, tab);
                }
            }
            Action::SaveScriptCommented(tab, comment) => {
                self.script_prompt = None;
                self.pending_comment = Some(comment);
                self.save_script(ctx, tab);
                self.pending_comment = None;
            }
            Action::SetRevisionComment(path, id, comment) => {
                let mut note = self.scripts.revision_note(&path, id);
                note.comment = comment.trim().to_owned();
                if let Err(error) = self.scripts.set_revision_note(&path, id, &note) {
                    self.toasts.push(
                        ctx,
                        Toast::error("Could not save the comment", error.to_string()),
                    );
                }
            }
            Action::OpenScriptFile => {
                let mut picker = rfd::FileDialog::new()
                    .set_title("Open script")
                    .add_filter("Scripts", scripts::EXTENSIONS);
                if let Some(folder) = self.last_script_folder() {
                    picker = picker.set_directory(folder);
                }
                if let Some(path) = picker.pick_file() {
                    self.open_script(ctx, path);
                }
            }
            Action::OpenScript(path) => self.open_script(ctx, path),
            Action::MoveScript(path, folder) => self.move_script(ctx, &path, &folder),
            Action::ScriptHistory(path) => self.show_script_history(ctx, path),
            Action::RestoreRevision(path, id) => self.restore_revision(ctx, path, id),
            Action::MoveScriptFolder(connection, database, folder, parent) => {
                self.move_script_folder(ctx, connection, &database, &folder, &parent);
            }
            _ => {}
        }
    }

    /// Moves a script of the scripts folder to another folder of its
    /// database; an editor open on it follows.
    fn move_script(&mut self, ctx: &egui::Context, path: &Path, folder: &str) {
        let Some(saved) = self.scripts.locate(path) else {
            return;
        };
        let Some(file_name) = path.file_name() else {
            return;
        };
        let to = self
            .scripts
            .folder_path(saved.connection, &saved.database, folder)
            .join(file_name);
        if to.exists() {
            self.toasts.push(
                ctx,
                Toast::error(
                    "Could not move the script",
                    format!("That folder already has a script named {}.", saved.name),
                ),
            );
            return;
        }
        if let Err(error) = scripts::rename(path, &to) {
            self.toasts.push(
                ctx,
                Toast::error("Could not move the script", error.to_string()),
            );
            return;
        }
        self.scripts.move_revisions(path, &to);
        for tab in &mut self.workspace.tabs {
            if let Some(file) = &mut tab.file
                && file.path == path
            {
                file.path.clone_from(&to);
            }
        }
        self.reload_scripts(ctx);
    }

    /// Moves a script folder into `parent` of its database; editors open
    /// on scripts in it follow.
    fn move_script_folder(
        &mut self,
        ctx: &egui::Context,
        connection: ConnectionId,
        database: &str,
        folder: &str,
        parent: &str,
    ) {
        let Some(to) = violet_app::moved_folder(folder, parent) else {
            return;
        };
        if self.scripts.folder_path(connection, database, &to).exists() {
            self.toasts
                .push(ctx, Toast::info("There is a folder with this name there"));
            return;
        }
        let (from_dir, to_dir) = match self.scripts.move_folder(connection, database, folder, &to) {
            Ok(moved) => moved,
            Err(error) => {
                self.toasts.push(
                    ctx,
                    Toast::error("Could not move the folder", error.to_string()),
                );
                return;
            }
        };
        for tab in &mut self.workspace.tabs {
            if let Some(file) = &mut tab.file
                && let Ok(rest) = file.path.strip_prefix(&from_dir)
            {
                file.path = to_dir.join(rest);
            }
        }
        let prefix = format!("{folder}/");
        let moved: Vec<NodeKey> = self
            .explorer
            .expanded
            .iter()
            .filter(|key| {
                matches!(key, NodeKey::ScriptFolder(id, db, path)
                    if *id == connection && db == database
                        && (path == folder || path.starts_with(&prefix)))
            })
            .cloned()
            .collect();
        for key in moved {
            self.explorer.expanded.remove(&key);
            if let NodeKey::ScriptFolder(id, db, path) = key {
                let path = format!("{to}{}", &path[folder.len()..]);
                self.explorer
                    .expanded
                    .insert(NodeKey::ScriptFolder(id, db, path));
            }
        }
        self.reload_scripts(ctx);
    }

    /// Makes a folder of scripts below `parent` of a database.
    pub(super) fn create_script_folder(
        &mut self,
        ctx: &egui::Context,
        connection: ConnectionId,
        database: &str,
        folder: &str,
    ) -> violet_core::Result<()> {
        self.scripts.create_folder(connection, database, folder)?;
        self.reload_scripts(ctx);
        Ok(())
    }

    fn save_script(&mut self, ctx: &egui::Context, tab_id: TabId) {
        let Some(tab) = self.workspace.get(tab_id) else {
            return;
        };
        if !matches!(tab.kind, TabKind::Script) {
            return;
        }
        let target = tab
            .connection
            .and_then(|session| self.script_target(session));
        match (tab.file.clone(), target) {
            (Some(file), target) => {
                // A script of the scripts folder follows its editor to
                // another connection or database.
                let path = match (self.scripts.locate(&file.path), target) {
                    (Some(saved), Some((connection, database)))
                        if (saved.connection, &saved.database) != (connection, &database) =>
                    {
                        let extension = file
                            .path
                            .extension()
                            .and_then(|e| e.to_str())
                            .unwrap_or(scripts::EXTENSION)
                            .to_owned();
                        let moved = self.scripts.path_in(
                            connection,
                            &database,
                            &saved.folder,
                            &saved.name,
                            &extension,
                        );
                        if moved.exists() {
                            self.toasts.push(
                                ctx,
                                Toast::error(
                                    "Could not move the script",
                                    format!(
                                        "{} already has a script named {}.",
                                        self.target_name(connection, &database),
                                        saved.name
                                    ),
                                ),
                            );
                            return;
                        }
                        if let Err(error) = scripts::rename(&file.path, &moved) {
                            self.toasts.push(
                                ctx,
                                Toast::error("Could not move the script", error.to_string()),
                            );
                            return;
                        }
                        self.scripts.move_revisions(&file.path, &moved);
                        moved
                    }
                    _ => file.path,
                };
                self.write_script(ctx, tab_id, path);
            }
            (None, Some((connection, database))) => {
                let name = tab.title.clone();
                self.script_prompt = Some(ScriptPrompt {
                    tab: tab_id,
                    name,
                    target: self.target_name(connection, &database),
                    error: None,
                    comment: String::new(),
                    ask_name: true,
                });
            }
            (None, None) => self.save_script_as(ctx, tab_id),
        }
    }

    fn save_script_named(&mut self, ctx: &egui::Context, tab_id: TabId, name: &str) {
        let name = name.trim();
        let target = self
            .workspace
            .get(tab_id)
            .and_then(|tab| tab.connection)
            .and_then(|session| self.script_target(session));
        let Some((connection, database)) = target else {
            self.script_prompt = None;
            return;
        };
        let refuse = |app: &mut Self, error: &str| {
            if let Some(prompt) = &mut app.script_prompt {
                prompt.error = Some(error.to_owned());
            }
        };
        if name.is_empty() {
            refuse(self, "Type a name.");
            return;
        }
        let extension = crate::lang::extension(self.language(connection));
        let path = self
            .scripts
            .path_for(connection, &database, name, extension);
        if path.exists() && self.workspace.script_tab(&path) != Some(tab_id) {
            refuse(self, "A script with this name is already saved here.");
            return;
        }
        self.script_prompt = None;
        self.write_script(ctx, tab_id, path);
    }

    fn save_script_as(&mut self, ctx: &egui::Context, tab_id: TabId) {
        let Some(tab) = self.workspace.get(tab_id) else {
            return;
        };
        if !matches!(tab.kind, TabKind::Script) {
            return;
        }
        let extension = crate::lang::extension(
            tab.connection
                .map_or(violet_core::Language::Sql, |id| self.language(id)),
        );
        let mut picker = rfd::FileDialog::new()
            .set_title("Save script as")
            .add_filter("Scripts", &[extension])
            .set_file_name(format!("{}.{extension}", tab.title));
        let folder = match &tab.file {
            Some(file) => file.path.parent().map(Path::to_path_buf),
            None => self.last_script_folder(),
        };
        if let Some(folder) = folder {
            picker = picker.set_directory(folder);
        }
        let Some(mut path) = picker.save_file() else {
            return;
        };
        if path.extension().is_none() {
            path.set_extension(extension);
        }
        self.write_script(ctx, tab_id, path);
    }

    /// Writes an editor's text to `path`, which becomes its file.
    fn write_script(&mut self, ctx: &egui::Context, tab_id: TabId, path: PathBuf) {
        let comment = self.pending_comment.take().unwrap_or_default();
        let Some(tab) = self.workspace.get_mut(tab_id) else {
            return;
        };
        if let Err(error) = scripts::write(&path, &tab.sql) {
            self.toasts.push(
                ctx,
                Toast::error("Could not save the script", error.to_string()),
            );
            return;
        }
        let note = violet_storage::RevisionNote {
            comment: comment.trim().to_owned(),
            performance: super::revisions_flow::performance_of(tab),
        };
        if let Err(error) = self.scripts.record_revision(&path, &tab.sql, &note) {
            log::warn!("script history: {error}");
        }
        tab.title = script_title(&self.scripts, &path);
        tab.file = Some(ScriptFile {
            path: path.clone(),
            saved: tab.sql.clone(),
        });
        let title = tab.title.clone();
        self.settings.script_folder = path
            .parent()
            .map(|folder| folder.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.save_settings(ctx);
        self.reload_scripts(ctx);
        self.toasts
            .push(ctx, Toast::success(format!("Saved {title}")));
    }

    /// Opens the script at `path`, or the editor already open on it. A
    /// script of the scripts folder opens on its database; any other file
    /// on the active editor's connection.
    pub(super) fn open_script(&mut self, ctx: &egui::Context, path: PathBuf) {
        if let Some(tab) = self.workspace.script_tab(&path) {
            self.workspace.active = Some(tab);
            return;
        }
        let sql = match scripts::read(&path) {
            Ok(sql) => sql,
            Err(error) => {
                self.toasts.push(
                    ctx,
                    Toast::error("Could not open the script", error.to_string()),
                );
                return;
            }
        };
        let connection = match self.scripts.locate(&path) {
            Some(saved) => self.database_session(saved.connection, &saved.database),
            None => self.workspace.active().and_then(|tab| tab.connection),
        };
        let title = script_title(&self.scripts, &path);
        let tab = self
            .workspace
            .open(title, connection, sql.clone(), TabKind::Script);
        if let Some(tab) = self.workspace.get_mut(tab) {
            tab.file = Some(ScriptFile { path, saved: sql });
        }
    }

    /// Reads the files of editors restored from the last session, so they
    /// show whether they have unsaved changes, and puts scripts of the
    /// scripts folder back on their database.
    pub(super) fn reattach_scripts(&mut self) {
        let files: Vec<(TabId, PathBuf)> = self
            .workspace
            .tabs
            .iter()
            .filter_map(|tab| Some((tab.id, tab.file.as_ref()?.path.clone())))
            .collect();
        for (tab_id, path) in files {
            let saved = scripts::read(&path).ok();
            let connection = self
                .scripts
                .locate(&path)
                .and_then(|saved| self.database_session(saved.connection, &saved.database));
            let Some(tab) = self.workspace.get_mut(tab_id) else {
                continue;
            };
            match saved {
                Some(saved) => {
                    if let Some(file) = &mut tab.file {
                        file.saved = saved;
                    }
                }
                // Gone since: the editor keeps its text, unsaved.
                None => tab.file = None,
            }
            if connection.is_some() {
                tab.connection = connection;
            }
        }
    }

    /// `connection · database`, as the user knows them.
    fn target_name(&self, connection: ConnectionId, database: &str) -> String {
        let name = self
            .profiles
            .get(connection)
            .map_or_else(|| "connection".to_owned(), |p| p.display_name());
        format!("{name} · {database}")
    }

    /// Where the last script was saved, else the scripts folder.
    fn last_script_folder(&self) -> Option<PathBuf> {
        let last = PathBuf::from(&self.settings.script_folder);
        if !self.settings.script_folder.is_empty() && last.is_dir() {
            return Some(last);
        }
        self.scripts
            .root()
            .is_dir()
            .then(|| self.scripts.root().to_path_buf())
    }
}

/// A tab's title for the script at `path`: its name as saved.
pub(super) fn script_title(store: &violet_storage::ScriptStore, path: &Path) -> String {
    store.locate(path).map_or_else(
        || {
            path.file_stem()
                .map_or_else(String::new, |stem| stem.to_string_lossy().into_owned())
        },
        |saved| saved.name,
    )
}

/// Asks for a script's name. Returns whether it is still open.
pub fn show_prompt(
    ctx: &egui::Context,
    prompt: &mut ScriptPrompt,
    actions: &mut Vec<Action>,
) -> bool {
    let palette = Palette::current(ctx);
    let mut open = true;
    let modal = egui::Modal::new(egui::Id::new("script-prompt"))
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
            let first_frame = ui.memory(|memory| memory.focused().is_none());
            let (title, hint) = if prompt.ask_name {
                (
                    "Save script",
                    format!("Saved for {}.", prompt.target),
                )
            } else {
                (
                    "Save with comment",
                    "The comment is kept with this version in the script's history.".to_owned(),
                )
            };
            ui.label(
                RichText::new(title)
                    .font(theme::semibold(16.0))
                    .color(palette.text),
            );
            ui.add(egui::Label::new(RichText::new(hint).color(palette.secondary)).wrap());
            let mut submit = false;
            if prompt.ask_name {
                let response = ui.add(widgets::text_input(&mut prompt.name, "Name"));
                if first_frame {
                    response.request_focus();
                }
                submit =
                    response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
            }
            let comment = ui.add(
                egui::TextEdit::multiline(&mut prompt.comment)
                    .hint_text("Comment (optional): what changed, why")
                    .desired_rows(3)
                    .margin(egui::vec2(10.0, 7.0))
                    .desired_width(f32::INFINITY),
            );
            if first_frame && !prompt.ask_name {
                comment.request_focus();
            }
            if comment.has_focus()
                && ui.input(|input| input.modifiers.command && input.key_pressed(egui::Key::Enter))
            {
                submit = true;
            }
            if let Some(error) = &prompt.error {
                ui.label(RichText::new(error).color(palette.danger));
            }
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if widgets::button(ui, Tone::Primary, Some(Icon::Save), "Save").clicked()
                        || submit
                    {
                        actions.push(if prompt.ask_name {
                            Action::SaveScriptNamed(
                                prompt.tab,
                                prompt.name.clone(),
                                prompt.comment.clone(),
                            )
                        } else {
                            Action::SaveScriptCommented(prompt.tab, prompt.comment.clone())
                        });
                    }
                    if widgets::button(ui, Tone::Ghost, None, "Cancel").clicked() {
                        open = false;
                    }
                });
            });
        });
    open && !modal.should_close()
}

/// Asks whether to save the scripts of the tabs being closed. `None`
/// while open.
pub fn confirm_close(ctx: &egui::Context, confirm: &CloseConfirm) -> Option<CloseAnswer> {
    let palette = Palette::current(ctx);
    let mut answer = None;
    let modal = egui::Modal::new(egui::Id::new("confirm-close"))
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
            let title = if confirm.quit {
                "Save changes before closing?"
            } else {
                "Save changes?"
            };
            ui.label(
                RichText::new(title)
                    .font(theme::semibold(16.0))
                    .color(palette.text),
            );
            let text = match confirm.names.as_slice() {
                [name] => format!(
                    "\u{201C}{name}\u{201D} has changes not saved to its file. They are lost if you don\u{2019}t save them."
                ),
                names => format!(
                    "{} scripts have changes not saved to their files: {}. They are lost if you don\u{2019}t save them.",
                    names.len(),
                    names.join(", ")
                ),
            };
            ui.add(egui::Label::new(RichText::new(text).color(palette.secondary)).wrap());
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if widgets::button(ui, Tone::Primary, Some(Icon::Save), "Save").clicked() {
                        answer = Some(CloseAnswer::Save);
                    }
                    if widgets::button(ui, Tone::Secondary, None, "Don\u{2019}t save").clicked() {
                        answer = Some(CloseAnswer::Discard);
                    }
                    if widgets::button(ui, Tone::Ghost, None, "Cancel").clicked() {
                        answer = Some(CloseAnswer::Cancel);
                    }
                });
            });
        });
    if modal.should_close() {
        answer = Some(CloseAnswer::Cancel);
    }
    answer
}
