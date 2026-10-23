//! Workspaces: sets of connections, open editors and scripts, one open at
//! a time. The code calls them projects, as [`super::workspace`] is the
//! open tabs; the user sees "workspace". The default one keeps the files
//! Violet had before workspaces.

use std::sync::Arc;

use violet_app::ProfileService;
use violet_storage::{JsonFile, JsonProfileRepository, ScriptStore};

use super::VioletApp;
use super::action::Action;
use super::state::Explorer;
use super::toasts::Toast;
use super::workspace::Workspace;
use crate::settings::Project;

impl VioletApp {
    pub(super) fn project_action(&mut self, ctx: &egui::Context, action: Action) {
        match action {
            Action::SwitchProject(id) => self.switch_project(ctx, id),
            Action::AskDeleteProject(id) => self.confirm_project_delete = Some(id),
            Action::DeleteProject(id) => self.delete_project(ctx, &id),
            _ => {}
        }
    }

    /// Why the open workspace cannot be left now, if it cannot: closing it
    /// would lose work or cut something short.
    fn busy(&self) -> Option<&'static str> {
        let tabs = &self.workspace.tabs;
        if tabs.iter().any(|tab| tab.running.is_some()) || !self.applying.is_empty() {
            Some("A query is running.")
        } else if tabs.iter().any(super::workspace::QueryTab::has_changes) {
            Some("A grid has changes not saved.")
        } else if tabs.iter().any(super::workspace::QueryTab::unsaved) {
            Some("A script has changes not saved.")
        } else if !self.started.is_empty() {
            Some("A backup or restore is running.")
        } else if self
            .explorer
            .transactions
            .values()
            .any(|transaction| transaction.open)
        {
            Some("A transaction is open.")
        } else {
            None
        }
    }

    /// Closes the open workspace, its sessions and editors, and opens
    /// workspace `id` (`None` for the default one).
    fn switch_project(&mut self, ctx: &egui::Context, id: Option<String>) {
        if id.as_deref() == self.settings.project() {
            return;
        }
        if let Some(reason) = self.busy() {
            self.toasts
                .push(ctx, Toast::error("Cannot switch workspaces yet", reason));
            return;
        }
        self.save_session(true);
        let sessions: Vec<_> = self
            .explorer
            .links
            .keys()
            .chain(self.explorer.opened.keys())
            .chain(self.profiles.all().iter().map(|profile| &profile.id))
            .copied()
            .collect();
        for session in sessions {
            if self.backend.is_connected(session) {
                self.backend.disconnect(session);
            }
        }
        self.explorer = Explorer {
            schema_filters: std::mem::take(&mut self.explorer.schema_filters),
            database_filters: std::mem::take(&mut self.explorer.database_filters),
            languages: std::mem::take(&mut self.explorer.languages),
            ..Explorer::default()
        };
        self.workspace = Workspace::default();
        self.waiting.clear();
        self.queued.clear();
        self.ending.clear();
        self.reference = None;
        self.form = None;
        self.prompt = None;
        self.confirm_delete = None;

        let paths = self.dirs.project(id.as_deref());
        let repository = Arc::new(JsonProfileRepository::new(&paths.profiles_file));
        let (profiles, error) = ProfileService::load(repository, self.profiles.secrets());
        self.profiles = profiles;
        if let Some(error) = error {
            self.toasts.push(
                ctx,
                Toast::error("Saved connections could not be read", error.to_string()),
            );
        }
        self.session_file = JsonFile::new(paths.session_file);
        self.session_saved = 0;
        self.scripts = ScriptStore::new(paths.scripts_dir);
        self.settings.active_project = id;
        self.save_settings(ctx);
        self.names_unchecked = true;

        self.restore_session(ctx);
        self.reattach_scripts();
        self.reload_scripts(ctx);
    }

    /// Adds a workspace named `name` and opens it.
    pub(super) fn create_project(&mut self, ctx: &egui::Context, name: &str) {
        let id = uuid::Uuid::new_v4().simple().to_string();
        self.settings.projects.push(Project {
            id: id.clone(),
            name: name.to_owned(),
        });
        self.save_settings(ctx);
        self.switch_project(ctx, Some(id));
    }

    pub(super) fn rename_project(&mut self, ctx: &egui::Context, id: &str, name: &str) {
        for project in self.settings.projects.iter_mut().filter(|p| p.id == id) {
            name.clone_into(&mut project.name);
        }
        self.save_settings(ctx);
    }

    /// Deletes a workspace that is not open, with its connections'
    /// passwords and its files.
    fn delete_project(&mut self, ctx: &egui::Context, id: &str) {
        if self.settings.project() == Some(id) {
            return;
        }
        let paths = self.dirs.project(Some(id));
        let repository = Arc::new(JsonProfileRepository::new(&paths.profiles_file));
        let (mut profiles, _) = ProfileService::load(repository, self.profiles.secrets());
        let ids: Vec<_> = profiles.all().iter().map(|profile| profile.id).collect();
        for connection in ids {
            if let Err(error) = profiles.remove(connection) {
                log::warn!("workspace {id}: {error}");
            }
        }
        for file in [
            &paths.profiles_file,
            &paths.session_file,
            &paths.scripts_dir,
        ] {
            if let Some(folder) = file.parent()
                && let Err(error) = std::fs::remove_dir_all(folder)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                log::warn!("workspace {id}: {error}");
            }
        }
        self.settings.projects.retain(|project| project.id != id);
        self.save_settings(ctx);
    }
}
