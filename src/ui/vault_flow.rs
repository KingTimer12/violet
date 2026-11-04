//! The master password: asked for at start when it guards the saved
//! passwords, and set, changed or removed on request.

use egui::{CornerRadius, RichText, Stroke};
use violet_core::Secret;
use violet_storage::MIN_MASTER_PASSWORD;
use zeroize::Zeroize as _;

use super::VioletApp;
use super::action::Action;
use super::icons::Icon;
use super::theme::{self, Palette};
use super::toasts::Toast;
use super::widgets::{self, Tone};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The vault is locked: ask for the master password.
    Unlock,
    /// Set one, or change or remove the one there is.
    Manage { has_password: bool },
}

pub struct VaultDialog {
    mode: Mode,
    current: String,
    new: String,
    confirm: String,
    error: Option<String>,
}

impl Drop for VaultDialog {
    fn drop(&mut self) {
        self.current.zeroize();
        self.new.zeroize();
        self.confirm.zeroize();
    }
}

impl VaultDialog {
    pub fn new(mode: Mode) -> Self {
        Self {
            mode,
            current: String::new(),
            new: String::new(),
            confirm: String::new(),
            error: None,
        }
    }

    /// Why the new password cannot be set, if it cannot.
    fn new_problem(&self) -> Option<String> {
        if self.new.chars().count() < MIN_MASTER_PASSWORD {
            Some(format!("Use at least {MIN_MASTER_PASSWORD} characters."))
        } else if self.new != self.confirm {
            Some("The passwords differ.".to_owned())
        } else {
            None
        }
    }
}

impl VioletApp {
    pub(super) fn vault_action(&mut self, ctx: &egui::Context, action: Action) {
        let Some(vault) = self.vault.clone() else {
            return;
        };
        match action {
            Action::MasterPassword => {
                let mode = if vault.is_locked() {
                    Mode::Unlock
                } else {
                    Mode::Manage {
                        has_password: vault.has_master_password(),
                    }
                };
                self.vault_dialog = Some(VaultDialog::new(mode));
            }
            Action::UnlockVault => {
                let Some(dialog) = &mut self.vault_dialog else {
                    return;
                };
                match vault.unlock(&Secret::new(dialog.current.clone())) {
                    Ok(()) => {
                        self.vault_dialog = None;
                        self.toasts
                            .push(ctx, Toast::success("Saved passwords unlocked"));
                    }
                    Err(error) => {
                        dialog.current.zeroize();
                        dialog.error = Some(error.message);
                    }
                }
            }
            Action::SaveMasterPassword { remove } => {
                let Some(dialog) = &mut self.vault_dialog else {
                    return;
                };
                let current =
                    (!dialog.current.is_empty()).then(|| Secret::new(dialog.current.clone()));
                let new = (!remove).then(|| Secret::new(dialog.new.clone()));
                match vault.set_master_password(current.as_ref(), new.as_ref()) {
                    Ok(()) => {
                        self.vault_dialog = None;
                        let done = if remove {
                            "Master password removed; the key is back in the keychain"
                        } else {
                            "Master password set"
                        };
                        self.toasts.push(ctx, Toast::success(done));
                    }
                    Err(error) => {
                        dialog.current.zeroize();
                        dialog.error = Some(error.message);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Returns `false` once the dialog should close.
pub fn show(ctx: &egui::Context, dialog: &mut VaultDialog, actions: &mut Vec<Action>) -> bool {
    let palette = Palette::current(ctx);
    let mut open = true;
    let modal = egui::Modal::new(egui::Id::new("vault-dialog"))
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
            let text = |ui: &mut egui::Ui, body: &str| {
                ui.add(egui::Label::new(RichText::new(body).color(palette.secondary)).wrap());
            };
            let title = |ui: &mut egui::Ui, title: &str| {
                ui.label(
                    RichText::new(title)
                        .font(theme::semibold(16.0))
                        .color(palette.text),
                );
            };
            let enter = |ui: &egui::Ui, response: &egui::Response| {
                response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter))
            };
            match dialog.mode {
                Mode::Unlock => {
                    title(ui, "Unlock saved passwords");
                    text(
                        ui,
                        "A master password guards the passwords Violet keeps. Enter it to \
                         connect with them.",
                    );
                    let field = ui.add(
                        widgets::text_input(&mut dialog.current, "Master password").password(true),
                    );
                    if dialog.current.is_empty() {
                        field.request_focus();
                    }
                    if let Some(error) = &dialog.error {
                        ui.label(RichText::new(error).color(palette.danger));
                    }
                    let submit = enter(ui, &field);
                    ui.horizontal(|ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let ready = !dialog.current.is_empty();
                            let go = ui.add_enabled_ui(ready, |ui| {
                                widgets::button(ui, Tone::Primary, Some(Icon::Lock), "Unlock")
                            });
                            if go.inner.clicked() || (submit && ready) {
                                actions.push(Action::UnlockVault);
                            }
                            if widgets::button(ui, Tone::Ghost, None, "Later").clicked() {
                                open = false;
                            }
                        });
                    });
                }
                Mode::Manage { has_password } => {
                    title(ui, "Master password");
                    text(
                        ui,
                        if has_password {
                            "Change the master password, or remove it to keep the key of the \
                             saved passwords in the system keychain again."
                        } else {
                            "With a master password, the key of the saved passwords leaves the \
                             system keychain, and Violet asks for the password each time it \
                             starts. Forget it and the saved passwords are lost."
                        },
                    );
                    if has_password {
                        ui.add(
                            widgets::text_input(&mut dialog.current, "Current master password")
                                .password(true),
                        );
                    }
                    ui.add(
                        widgets::text_input(&mut dialog.new, "New master password").password(true),
                    );
                    let last = ui.add(
                        widgets::text_input(&mut dialog.confirm, "Repeat the new password")
                            .password(true),
                    );
                    let problem = dialog.new_problem();
                    if let Some(error) = &dialog.error {
                        ui.add(egui::Label::new(RichText::new(error).color(palette.danger)).wrap());
                    } else if let Some(problem) = &problem
                        && !dialog.new.is_empty()
                    {
                        ui.label(RichText::new(problem).color(palette.secondary));
                    }
                    let current_ok = !has_password || !dialog.current.is_empty();
                    let ready = current_ok && problem.is_none();
                    let submit = enter(ui, &last);
                    ui.horizontal(|ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let verb = if has_password { "Change" } else { "Set" };
                            let go = ui.add_enabled_ui(ready, |ui| {
                                widgets::button(ui, Tone::Primary, Some(Icon::Lock), verb)
                            });
                            if go.inner.clicked() || (submit && ready) {
                                actions.push(Action::SaveMasterPassword { remove: false });
                            }
                            if has_password {
                                let remove = ui.add_enabled_ui(current_ok, |ui| {
                                    widgets::button(ui, Tone::Secondary, None, "Remove")
                                });
                                if remove.inner.clicked() {
                                    actions.push(Action::SaveMasterPassword { remove: true });
                                }
                            }
                            if widgets::button(ui, Tone::Ghost, None, "Cancel").clicked() {
                                open = false;
                            }
                        });
                    });
                }
            }
        });
    open && !modal.should_close()
}
