//! Changing the keyboard shortcuts: a dialog listing every command with
//! its shortcut; clicking one waits for the keys to press instead.

use egui::{Key, KeyboardShortcut, Modifiers, RichText};

use super::VioletApp;
use super::action::Action;
use super::icons::Icon;
use super::shortcuts::{self, Command};
use super::theme::{self, Palette};
use super::widgets::{self, Tone};

/// The shortcuts dialog.
#[derive(Default)]
pub struct KeysDialog {
    /// The command waiting for its new keys.
    capturing: Option<Command>,
    /// Why the keys pressed last were refused.
    error: Option<String>,
}

/// Shortcuts the text fields and the grid keep for themselves.
fn reserved(shortcut: KeyboardShortcut) -> bool {
    let modifiers = shortcut.modifiers;
    modifiers.command
        && !modifiers.alt
        && matches!(
            (shortcut.logical_key, modifiers.shift),
            (
                Key::A | Key::C | Key::V | Key::X | Key::Z | Key::Space,
                false
            ) | (Key::Z | Key::C, true)
        )
}

impl VioletApp {
    /// Takes the keys pressed this frame for the command waiting for them,
    /// before anything else reads them.
    pub(super) fn capture_shortcut(&mut self, ctx: &egui::Context) {
        let Some(dialog) = &mut self.keys_dialog else {
            return;
        };
        let Some(command) = dialog.capturing else {
            return;
        };
        let pressed = ctx.input_mut(|input| {
            // A modifier the event lost may still be held, and the other way
            // round: some systems (Windows under WSLg, for one) take
            // combinations such as Alt+Shift on their way.
            let now = input.modifiers;
            // Shift, Ctrl and Alt come as keys of their own too: the
            // shortcut is the first other key.
            let pressed = input.events.iter().find_map(|event| match event {
                egui::Event::Key {
                    key,
                    modifiers,
                    pressed: true,
                    ..
                } if !shortcuts::is_modifier(*key) => Some((*key, *modifiers | now)),
                _ => None,
            });
            input
                .events
                .retain(|event| !matches!(event, egui::Event::Key { .. }));
            pressed
        });
        let Some((key, held)) = pressed else {
            return;
        };
        log::info!("shortcut for {command:?}: {key:?} with {held:?}");
        if key == Key::Escape && held.is_none() {
            dialog.capturing = None;
            dialog.error = None;
            return;
        }
        let shortcut = KeyboardShortcut::new(
            Modifiers {
                alt: held.alt,
                shift: held.shift,
                command: held.command,
                ..Modifiers::NONE
            },
            key,
        );
        let function_key = key.name().len() > 1
            && key.name().starts_with('F')
            && key.name()[1..].chars().all(|c| c.is_ascii_digit());
        let error = if !(shortcut.modifiers.command || shortcut.modifiers.alt || function_key) {
            Some(format!(
                "Hold {} or Alt with the key: alone, it would type.",
                if cfg!(target_os = "macos") {
                    "⌘"
                } else {
                    "Ctrl"
                }
            ))
        } else if reserved(shortcut) {
            Some("Copying, pasting and undoing keep that one.".to_owned())
        } else {
            shortcuts::taken_by(shortcut)
                .filter(|other| *other != command)
                .map(|other| format!("Already used by \u{201C}{}\u{201D}.", other.label()))
        };
        match error {
            Some(error) => dialog.error = Some(error),
            None => {
                dialog.capturing = None;
                dialog.error = None;
                self.actions
                    .push(Action::SetShortcut(command, Some(shortcut)));
            }
        }
    }

    /// Keeps `command`'s shortcut, or its default with `None`.
    pub(super) fn set_shortcut(
        &mut self,
        ctx: &egui::Context,
        command: Command,
        shortcut: Option<KeyboardShortcut>,
    ) {
        match shortcut.filter(|shortcut| *shortcut != command.default_shortcut()) {
            Some(shortcut) => self
                .settings
                .shortcuts
                .insert(command.key().to_owned(), shortcuts::to_text(shortcut)),
            None => self.settings.shortcuts.remove(command.key()),
        };
        shortcuts::apply(&self.settings.shortcuts);
        self.save_settings(ctx);
    }
}

/// Shows the dialog. Returns whether it is still open.
pub fn show(ctx: &egui::Context, dialog: &mut KeysDialog, actions: &mut Vec<Action>) -> bool {
    let palette = Palette::current(ctx);
    let mut open = true;
    // The keys go to the dialog, not to a field left focused.
    if dialog.capturing.is_some() {
        ctx.memory_mut(|memory| {
            if let Some(focused) = memory.focused() {
                memory.surrender_focus(focused);
            }
        });
    }
    let modal = egui::Modal::new(egui::Id::new("keyboard-shortcuts"))
        .frame(super::edit_flow::frame(&palette))
        .show(ctx, |ui| {
            ui.set_width(460.0);
            ui.spacing_mut().item_spacing.y = 10.0;
            ui.label(
                RichText::new("Keyboard shortcuts")
                    .font(theme::semibold(16.0))
                    .color(palette.text),
            );
            ui.label(
                RichText::new("Click a shortcut, then press the keys to use instead.")
                    .color(palette.dim),
            );
            egui::ScrollArea::vertical()
                .max_height(420.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    for command in Command::ALL {
                        row(ui, &palette, dialog, command, actions);
                    }
                });
            if let Some(error) = &dialog.error {
                ui.label(RichText::new(error).color(palette.danger));
            }
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if widgets::button(ui, Tone::Primary, Some(Icon::Check), "Done").clicked() {
                        open = false;
                    }
                    let changed = Command::ALL
                        .iter()
                        .any(|command| shortcuts::get(*command) != command.default_shortcut());
                    if ui
                        .add_enabled_ui(changed, |ui| {
                            widgets::button(ui, Tone::Ghost, Some(Icon::Refresh), "Reset all")
                        })
                        .inner
                        .clicked()
                    {
                        for command in Command::ALL {
                            actions.push(Action::SetShortcut(command, None));
                        }
                        dialog.capturing = None;
                        dialog.error = None;
                    }
                });
            });
        });
    // Escape while waiting for keys is the dialog's own.
    open && (dialog.capturing.is_some() || !modal.should_close())
}

/// A command, its shortcut, and a reset when it is not the default.
fn row(
    ui: &mut egui::Ui,
    palette: &Palette,
    dialog: &mut KeysDialog,
    command: Command,
    actions: &mut Vec<Action>,
) {
    let current = shortcuts::get(command);
    let waiting = dialog.capturing == Some(command);
    ui.horizontal(|ui| {
        ui.label(RichText::new(command.label()).color(palette.text));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let changed = current != command.default_shortcut();
            let reset = format!(
                "Back to {}",
                ui.ctx().format_shortcut(&command.default_shortcut())
            );
            if ui
                .add_enabled_ui(changed, |ui| {
                    widgets::icon_button(ui, Icon::Refresh, &reset)
                })
                .inner
                .clicked()
            {
                actions.push(Action::SetShortcut(command, None));
            }
            let (tone, label) = if waiting {
                // The modifiers held so far, as they are pressed.
                let held = ui.input(|input| input.modifiers);
                let mut parts = Vec::new();
                if held.command {
                    parts.push(if cfg!(target_os = "macos") {
                        "⌘"
                    } else {
                        "Ctrl"
                    });
                }
                if held.alt {
                    parts.push("Alt");
                }
                if held.shift {
                    parts.push("Shift");
                }
                parts.push("…");
                ui.ctx().request_repaint();
                (Tone::Primary, format!("{} (Esc cancels)", parts.join("+")))
            } else {
                (Tone::Secondary, ui.ctx().format_shortcut(&current))
            };
            if widgets::button(ui, tone, None, &label)
                .on_hover_text("Click, then press the new shortcut")
                .clicked()
            {
                dialog.capturing = (!waiting).then_some(command);
                dialog.error = None;
            }
        });
    });
}
