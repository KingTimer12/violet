//! Backup and restore, between the dialog and the backend.

use std::path::PathBuf;

use violet_app::Event;
use violet_core::{ConnectionId, DriverId, ObjectRef, Secret, TaskKind};

use super::VioletApp;
use super::action::Action;
use super::state::Loadable;
use super::tasks::{self, RunState, TaskDialog};
use super::toasts::Toast;

/// A task still running, for its notice when the dialog is gone.
pub struct Started {
    pub connection: ConnectionId,
    pub kind: TaskKind,
    pub path: PathBuf,
}

/// A provider's downloaded client programs.
pub enum Tools {
    Downloading { label: String, size: u64 },
    Failed(String),
}

/// The key a task's last settings are saved under.
fn settings_key(driver: &DriverId, kind: TaskKind) -> String {
    format!("{driver}/{}", kind.label().to_lowercase())
}

/// Where backups go before the user picks: the documents folder.
fn default_folder() -> String {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    home.map(PathBuf::from)
        .map(|home| {
            let documents = home.join("Documents");
            if documents.is_dir() { documents } else { home }
        })
        .map(|folder| folder.to_string_lossy().into_owned())
        .unwrap_or_default()
}

impl VioletApp {
    pub(super) fn open_task(
        &mut self,
        connection: ConnectionId,
        kind: TaskKind,
        objects: Vec<ObjectRef>,
    ) {
        let Some(profile) = self.profiles.get(connection) else {
            return;
        };
        let driver = profile.driver.clone();
        let mut dialog = TaskDialog::new(connection, driver.clone(), kind, objects);
        dialog.folder = if self.settings.backup_folder.is_empty() {
            default_folder()
        } else {
            self.settings.backup_folder.clone()
        };
        if !self.settings.backup_name.is_empty() {
            dialog.name.clone_from(&self.settings.backup_name);
        }
        if let Some(folder) = self.settings.client_folders.get(driver.as_str()) {
            dialog.client.clone_from(folder);
        }
        if !matches!(self.tasks.get(&driver), Some(Loadable::Ready(_))) {
            self.tasks.insert(driver.clone(), Loadable::Loading);
            self.backend.load_tasks(driver);
        }
        self.task_dialog = Some(dialog);
        self.initialize_task();
    }

    /// Fills the dialog's settings once its driver's tasks are known.
    fn initialize_task(&mut self) {
        let Some(dialog) = &mut self.task_dialog else {
            return;
        };
        let Some(Loadable::Ready(offer)) = self.tasks.get(&dialog.driver) else {
            return;
        };
        if let Some(task) = offer.tasks.iter().find(|task| task.kind == dialog.kind) {
            let last = self
                .settings
                .task_settings
                .get(&settings_key(&dialog.driver, dialog.kind));
            dialog.initialize(task, last);
        }
    }

    pub(super) fn task_action(&mut self, ctx: &egui::Context, action: Action) {
        match action {
            Action::OpenTask {
                connection,
                kind,
                objects,
            } => self.open_task(connection, kind, objects),
            Action::TrustTools(driver) => {
                self.settings.trusted_tools.insert(driver.to_string());
                self.save_settings(ctx);
            }
            Action::TaskFindPrograms => {
                let Some(dialog) = &self.task_dialog else {
                    return;
                };
                if let Some(Loadable::Ready(offer)) = self.tasks.get(&dialog.driver)
                    && let Some(task) = offer.tasks.iter().find(|task| task.kind == dialog.kind)
                {
                    self.backend.find_programs(
                        dialog.driver.clone(),
                        task.clone(),
                        dialog.client_folder(),
                    );
                }
            }
            Action::TaskPreview => {
                let Some((profile, request, folder)) = self.task_request() else {
                    return;
                };
                let id = self.backend.preview_task(profile, request, folder);
                if let Some(dialog) = &mut self.task_dialog {
                    dialog.preview_request = Some(id);
                }
            }
            Action::TaskInspect(path) => {
                if let Some(dialog) = &self.task_dialog {
                    self.backend.inspect_input(dialog.driver.clone(), path);
                }
            }
            Action::TaskStart => self.start_task(ctx),
            Action::TaskCancel => {
                if let Some(run) = self.task_dialog.as_ref().and_then(|d| d.run.as_ref()) {
                    self.backend.cancel_task(run.id);
                }
            }
            Action::TaskAgain => {
                if let Some(dialog) = &mut self.task_dialog {
                    dialog.reset();
                }
            }
            Action::TaskClose => self.task_dialog = None,
            Action::RetryTools(driver) => {
                self.tools.remove(&driver);
                self.backend.load_tasks(driver);
            }
            Action::Reveal(path) => tasks::reveal(&path),
            _ => {}
        }
    }

    /// The dialog's request as it would run now, with its profile.
    fn task_request(
        &self,
    ) -> Option<(
        violet_core::ConnectionProfile,
        violet_core::TaskRequest,
        Option<PathBuf>,
    )> {
        let dialog = self.task_dialog.as_ref()?;
        let profile = self.profiles.get(dialog.connection)?.clone();
        let Some(Loadable::Ready(offer)) = self.tasks.get(&dialog.driver) else {
            return None;
        };
        let task = offer.tasks.iter().find(|task| task.kind == dialog.kind)?;
        let request = dialog.request(task, &profile)?;
        Some((profile, request, dialog.client_folder()))
    }

    fn start_task(&mut self, ctx: &egui::Context) {
        let Some((profile, request, folder)) = self.task_request() else {
            return;
        };
        let Some(dialog) = &mut self.task_dialog else {
            return;
        };
        // One run per dialog: a second would race the first on the same
        // file or database.
        if dialog.running() || !self.settings.trusted_tools.contains(dialog.driver.as_str()) {
            return;
        }
        // What ran is what the dialog opens with next time.
        let key = settings_key(&dialog.driver, dialog.kind);
        self.settings
            .task_settings
            .insert(key, request.settings.clone());
        if dialog.kind == TaskKind::Backup {
            dialog
                .folder
                .trim()
                .clone_into(&mut self.settings.backup_folder);
            dialog
                .name
                .trim()
                .clone_into(&mut self.settings.backup_name);
        }
        match dialog.client_folder() {
            Some(client) => {
                self.settings.client_folders.insert(
                    dialog.driver.to_string(),
                    client.to_string_lossy().into_owned(),
                );
            }
            None => {
                self.settings.client_folders.remove(dialog.driver.as_str());
            }
        }
        let password = (!dialog.password.is_empty()).then(|| Secret::new(dialog.password.clone()));
        let path = PathBuf::from(&request.path);
        let id = self.backend.run_task(profile, request, folder, password);
        dialog.run = Some(RunState::new(id, path.clone()));
        self.started.insert(
            id,
            Started {
                connection: dialog.connection,
                kind: dialog.kind,
                path,
            },
        );
        self.save_settings(ctx);
    }

    pub(super) fn task_event(&mut self, ctx: &egui::Context, event: Event) {
        match event {
            Event::Tasks { driver, result } => {
                self.tasks.insert(driver, Loadable::from_result(result));
                self.initialize_task();
            }
            Event::ToolsDownloading {
                driver,
                label,
                size,
            } => {
                self.tools
                    .insert(driver, Tools::Downloading { label, size });
            }
            Event::ToolsReady { driver, result } => {
                let label = match self.tools.remove(&driver) {
                    Some(Tools::Downloading { label, .. }) => label,
                    _ => "Client programs".to_owned(),
                };
                match result {
                    Ok(Some(_)) => {
                        self.toasts
                            .push(ctx, Toast::success(format!("{label} downloaded")));
                    }
                    Ok(None) => {}
                    Err(error) => {
                        self.toasts.push(
                            ctx,
                            Toast::error(
                                format!("Could not download {label}"),
                                error.message.clone(),
                            ),
                        );
                        self.tools
                            .insert(driver.clone(), Tools::Failed(error.message));
                    }
                }
                // Look again: the programs may be there now.
                if let Some(dialog) = &mut self.task_dialog
                    && dialog.driver == driver
                {
                    dialog.refresh();
                }
            }
            Event::ProgramsFound {
                driver,
                kind,
                programs,
            } => {
                if let Some(dialog) = &mut self.task_dialog
                    && dialog.driver == driver
                    && dialog.kind == kind
                {
                    dialog.programs = Some(programs);
                }
            }
            Event::TaskPreview { request, result } => {
                if let Some(dialog) = &mut self.task_dialog
                    && dialog.preview_request == Some(request)
                {
                    dialog.preview = Some(result.map_err(|error| error.message));
                }
            }
            Event::InputInspected { path, result } => match result {
                Ok(settings) => {
                    if let Some(dialog) = &mut self.task_dialog {
                        dialog.apply_inspection(&path, settings);
                    }
                }
                Err(error) => log::warn!("inspect {}: {error}", path.display()),
            },
            Event::TaskOutput { task, lines } => {
                if let Some(run) = self
                    .task_dialog
                    .as_mut()
                    .and_then(|d| d.run.as_mut())
                    .filter(|run| run.id == task)
                {
                    run.push(lines);
                }
            }
            Event::TaskFinished { task, result } => {
                let started = self.started.remove(&task);
                let shown = self
                    .task_dialog
                    .as_mut()
                    .and_then(|d| d.run.as_mut())
                    .filter(|run| run.id == task);
                let message = result.as_ref().err().map(|error| error.message.clone());
                let cancelled = result
                    .as_ref()
                    .is_err_and(|error| error.kind == violet_core::ErrorKind::Cancelled);
                if let Some(run) = shown {
                    run.result = Some(result.map_err(|error| error.message));
                }
                let Some(started) = started else {
                    return;
                };
                let name = self.profile_name(started.connection);
                let what = started.kind.label();
                let toast = match message {
                    None => Toast::success(format!("{what} of {name} finished")),
                    Some(_) if cancelled => Toast::info(format!("{what} of {name} stopped")),
                    Some(message) => Toast::error(format!("{what} of {name} failed"), message),
                };
                self.toasts.push(ctx, toast);
                // A restore changed the catalog the tree shows.
                if started.kind == TaskKind::Restore
                    && !cancelled
                    && self.explorer.link(started.connection).is_online()
                {
                    self.actions.push(Action::Refresh(started.connection));
                }
                log::info!("{what} of {name} ({}) ended", started.path.display());
            }
            _ => {}
        }
    }

    pub(super) fn task_modal(&mut self, ctx: &egui::Context) {
        let Some(dialog) = &mut self.task_dialog else {
            return;
        };
        let Some(profile) = self.profiles.get(dialog.connection) else {
            self.task_dialog = None;
            return;
        };
        let driver_name = self
            .backend
            .registry()
            .get(&dialog.driver)
            .map_or_else(|| dialog.driver.to_string(), |driver| driver.info().name);
        let env = tasks::Env {
            profile,
            offer: self.tasks.get(&dialog.driver),
            tools: self.tools.get(&dialog.driver),
            explorer: &self.explorer,
            trusted: self.settings.trusted_tools.contains(dialog.driver.as_str()),
            driver_name: &driver_name,
        };
        if !tasks::show(ctx, dialog, &env, &mut self.actions) {
            // A task keeps running without its dialog; a notice tells how it
            // ends.
            self.task_dialog = None;
        }
    }
}
