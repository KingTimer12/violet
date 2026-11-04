//! Workspace backups: the open workspace exported to an encrypted
//! `.dbv2bak` file, or such a file imported into it.
//!
//! Deriving the key (Argon2id) takes a moment and a few dozen megabytes,
//! so the work runs on a thread of its own and the dialog waits for it.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::Duration;

use egui::{CornerRadius, RichText, Stroke};
use violet_backup::workspace::{self, Source};
use violet_core::Secret;
use zeroize::Zeroize as _;

use super::VioletApp;
use super::action::Action;
use super::icons::Icon;
use super::theme::{self, Palette};
use super::toasts::Toast;
use super::widgets::{self, Tone};

/// Shortest export password accepted: the file may travel anywhere, and
/// only the password keeps what it holds.
const MIN_PASSWORD: usize = 12;

pub enum Mode {
    Export,
    /// The backup file picked.
    Import(PathBuf),
}

/// What the thread hands back.
enum Outcome {
    Exported(PathBuf),
    Opened(violet_backup::Bundle),
    Failed(String),
}

/// The backup dialog: the password, and the work once it runs.
pub struct BackupDialog {
    mode: Mode,
    password: String,
    confirm: String,
    error: Option<String>,
    running: Option<Receiver<Outcome>>,
}

impl Drop for BackupDialog {
    fn drop(&mut self) {
        self.password.zeroize();
        self.confirm.zeroize();
    }
}

impl BackupDialog {
    fn new(mode: Mode) -> Self {
        Self {
            mode,
            password: String::new(),
            confirm: String::new(),
            error: None,
            running: None,
        }
    }

    /// Why the password typed cannot be used, if it cannot.
    fn problem(&self) -> Option<&'static str> {
        match self.mode {
            Mode::Export if self.password.chars().count() < MIN_PASSWORD => {
                Some("Use at least 12 characters.")
            }
            Mode::Export if self.password != self.confirm => Some("The passwords differ."),
            Mode::Import(_) if self.password.is_empty() => Some("Enter the backup's password."),
            _ => None,
        }
    }
}

impl VioletApp {
    pub(super) fn backup_action(&mut self, action: Action) {
        match action {
            Action::ExportProject => self.backup = Some(BackupDialog::new(Mode::Export)),
            Action::ImportProject => {
                let picker = rfd::FileDialog::new()
                    .set_title("Import workspace backup")
                    .add_filter("Violet backup", &[violet_backup::EXTENSION]);
                if let Some(path) = picker.pick_file() {
                    self.backup = Some(BackupDialog::new(Mode::Import(path)));
                }
            }
            Action::RunBackup => self.run_backup(),
            _ => {}
        }
    }

    fn run_backup(&mut self) {
        let Some(dialog) = &mut self.backup else {
            return;
        };
        if dialog.running.is_some() || dialog.problem().is_some() {
            return;
        }
        let password = Secret::new(dialog.password.clone());
        let (send, receive) = mpsc::channel();
        match &dialog.mode {
            Mode::Export => {
                let name = format!(
                    "{}.{}",
                    self.settings.project_name(),
                    violet_backup::EXTENSION
                );
                let Some(path) = rfd::FileDialog::new()
                    .set_title("Export workspace")
                    .set_file_name(name)
                    .add_filter("Violet backup", &[violet_backup::EXTENSION])
                    .save_file()
                else {
                    return;
                };
                let saved = self.profiles.saved();
                let secrets = self.profiles.secrets();
                let scripts = self.scripts.root().to_path_buf();
                std::thread::spawn(move || {
                    let source = Source {
                        profiles: &saved,
                        secrets: &*secrets,
                        scripts_dir: &scripts,
                        app_version: env!("CARGO_PKG_VERSION"),
                    };
                    let outcome = workspace::export(&source, &password)
                        .and_then(|file| workspace::write_file(&path, &file))
                        .map_or_else(
                            |error| Outcome::Failed(error.to_string()),
                            |()| Outcome::Exported(path),
                        );
                    let _ = send.send(outcome);
                });
            }
            Mode::Import(path) => {
                let path = path.clone();
                std::thread::spawn(move || {
                    let outcome = workspace::read_file(&path)
                        .and_then(|file| workspace::open(&file, &password))
                        .map_or_else(|error| Outcome::Failed(error.to_string()), Outcome::Opened);
                    let _ = send.send(outcome);
                });
            }
        }
        dialog.error = None;
        dialog.running = Some(receive);
    }

    /// Takes the thread's answer once it is there.
    pub(super) fn poll_backup(&mut self, ctx: &egui::Context) {
        let Some(receive) = self.backup.as_ref().and_then(|d| d.running.as_ref()) else {
            return;
        };
        let outcome = match receive.try_recv() {
            Ok(outcome) => outcome,
            Err(TryRecvError::Empty) => {
                ctx.request_repaint_after(Duration::from_millis(100));
                return;
            }
            Err(TryRecvError::Disconnected) => {
                Outcome::Failed("The backup stopped unexpectedly".to_owned())
            }
        };
        match outcome {
            Outcome::Exported(path) => {
                self.backup = None;
                let name = path
                    .file_name()
                    .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
                self.toasts
                    .push(ctx, Toast::success(format!("Workspace exported to {name}")));
            }
            Outcome::Opened(bundle) => {
                self.backup = None;
                self.apply_backup(ctx, &bundle);
            }
            Outcome::Failed(error) => {
                if let Some(dialog) = &mut self.backup {
                    dialog.running = None;
                    dialog.password.zeroize();
                    dialog.error = Some(error);
                }
            }
        }
    }

    /// Writes an opened backup into the open workspace.
    fn apply_backup(&mut self, ctx: &egui::Context, bundle: &violet_backup::Bundle) {
        let files = match workspace::restore_files(self.scripts.root(), &bundle.files) {
            Ok(files) => files,
            Err(error) => {
                self.toasts.push(
                    ctx,
                    Toast::error(
                        "The backup's scripts could not be restored",
                        error.to_string(),
                    ),
                );
                return;
            }
        };
        let connections = match self.profiles.import(&bundle.profiles, &bundle.passwords) {
            Ok(connections) => connections,
            Err(error) => {
                self.toasts.push(
                    ctx,
                    Toast::error("The backup's connections could not be saved", error.message),
                );
                return;
            }
        };
        self.names_unchecked = true;
        self.reload_scripts(ctx);
        self.toasts.push(
            ctx,
            Toast::success(format!(
                "Imported {connections} connection{} and {files} file{}",
                if connections == 1 { "" } else { "s" },
                if files == 1 { "" } else { "s" },
            )),
        );
    }
}

/// Returns `false` once the dialog should close.
pub fn show(ctx: &egui::Context, dialog: &mut BackupDialog, actions: &mut Vec<Action>) -> bool {
    let palette = Palette::current(ctx);
    let mut open = true;
    let running = dialog.running.is_some();
    let modal = egui::Modal::new(egui::Id::new("backup-dialog"))
        .frame(
            egui::Frame::new()
                .fill(palette.surface)
                .stroke(Stroke::new(1.0, palette.outline))
                .corner_radius(CornerRadius::same(14))
                .inner_margin(egui::Margin::same(22)),
        )
        .show(ctx, |ui| {
            ui.set_width(420.0);
            ui.spacing_mut().item_spacing.y = 12.0;
            let (title, body, verb) = match &dialog.mode {
                Mode::Export => (
                    "Export workspace",
                    "Its connections, their saved passwords and its scripts go into one \
                     encrypted file. Anyone with the file and this password can read them: \
                     keep the password apart from the file."
                        .to_owned(),
                    "Export",
                ),
                Mode::Import(path) => (
                    "Import workspace backup",
                    format!(
                        "Connections and scripts from {} are added to this workspace; the ones \
                         it shares with it are replaced.",
                        path.file_name()
                            .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
                    ),
                    "Import",
                ),
            };
            ui.label(
                RichText::new(title)
                    .font(theme::semibold(16.0))
                    .color(palette.text),
            );
            ui.add(egui::Label::new(RichText::new(body).color(palette.secondary)).wrap());
            let first = ui.add_enabled(
                !running,
                widgets::text_input(&mut dialog.password, "Password").password(true),
            );
            if !running && dialog.password.is_empty() && dialog.confirm.is_empty() {
                first.request_focus();
            }
            let mut last = first;
            if matches!(dialog.mode, Mode::Export) {
                last = ui.add_enabled(
                    !running,
                    widgets::text_input(&mut dialog.confirm, "Repeat the password").password(true),
                );
            }
            let problem = dialog.problem();
            if let Some(error) = &dialog.error {
                ui.add(egui::Label::new(RichText::new(error).color(palette.danger)).wrap());
            } else if let Some(problem) = problem
                && !dialog.password.is_empty()
            {
                ui.label(RichText::new(problem).color(palette.secondary));
            }
            let submit = last.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if running {
                        ui.spinner();
                        ui.label(RichText::new("Working…").color(palette.secondary));
                        return;
                    }
                    let go = ui.add_enabled_ui(problem.is_none(), |ui| {
                        widgets::button(ui, Tone::Primary, Some(Icon::Lock), verb)
                    });
                    if go.inner.clicked() || (submit && problem.is_none()) {
                        actions.push(Action::RunBackup);
                    }
                    if widgets::button(ui, Tone::Ghost, None, "Cancel").clicked() {
                        open = false;
                    }
                });
            });
        });
    // The work cannot be stopped halfway; the dialog stays until it ends.
    open && (running || !modal.should_close())
}
