//! Commits, rollbacks and the auto-commit switch.
//!
//! Connections start in auto-commit: each statement is committed as it
//! runs. In manual commit the backend opens a transaction before the first
//! statement that may write, and what it writes waits for Commit or
//! Rollback. Leaving manual commit, disconnecting or quitting with a
//! transaction open asks first what becomes of it.

use egui::{CornerRadius, RichText, Stroke};
use violet_app::TxCommand;
use violet_core::{ConnectionId, Result};

use super::VioletApp;
use super::icons::Icon;
use super::theme::{self, Palette};
use super::toasts::Toast;
use super::widgets::{self, Tone};

/// What waits for open transactions to end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Then {
    /// Switching the connection to auto-commit.
    AutoCommit,
    /// Disconnecting this connection, with the databases opened under it.
    Disconnect(ConnectionId),
    /// Closing the window.
    Quit,
}

/// Asks what becomes of open transactions.
pub struct TxConfirm {
    connections: Vec<ConnectionId>,
    /// Each one's name and what it wrote, as shown.
    names: Vec<String>,
    then: Then,
}

/// What to do with the open transactions.
#[derive(Clone, Copy)]
pub enum TxAnswer {
    Commit,
    Rollback,
    Cancel,
}

impl VioletApp {
    /// The connections among `ids` with a transaction open.
    fn open_transactions(&self, ids: impl IntoIterator<Item = ConnectionId>) -> Vec<ConnectionId> {
        ids.into_iter()
            .filter(|id| self.explorer.transaction(*id).open && self.backend.is_connected(*id))
            .collect()
    }

    fn ask_transactions(&mut self, connections: Vec<ConnectionId>, then: Then) {
        let names = connections
            .iter()
            .map(|id| {
                let writes = self.explorer.transaction(*id).writes;
                match writes {
                    0 => self.profile_name(*id),
                    1 => format!("{} (1 change)", self.profile_name(*id)),
                    n => format!("{} ({n} changes)", self.profile_name(*id)),
                }
            })
            .collect();
        self.tx_confirm = Some(TxConfirm {
            connections,
            names,
            then,
        });
    }

    pub(super) fn transact(&mut self, id: ConnectionId, command: TxCommand) {
        if command == TxCommand::AutoCommit(true) && self.explorer.transaction(id).open {
            self.ask_transactions(vec![id], Then::AutoCommit);
            return;
        }
        self.backend.transaction(id, command);
    }

    /// Disconnects `id`, asking first when it, or a database opened under
    /// it, has a transaction open.
    pub(super) fn disconnect_asking(&mut self, id: ConnectionId) {
        let mut ids = self.explorer.opened_under(id);
        ids.push(id);
        let open = self.open_transactions(ids);
        if open.is_empty() {
            self.disconnect(id);
        } else {
            self.ask_transactions(open, Then::Disconnect(id));
        }
    }

    /// Keeps the window open when it is asked to close with transactions
    /// open, and asks about them. Closing to install an update asks
    /// nothing.
    pub(super) fn keeps_transactions(&mut self, ctx: &egui::Context) -> bool {
        if self.tx_settled
            || self
                .updates
                .as_ref()
                .is_some_and(crate::update::Updates::should_quit)
        {
            return false;
        }
        let open = self.open_transactions(self.explorer.transactions.keys().copied());
        if open.is_empty() {
            return false;
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        if self.tx_confirm.is_none() {
            self.ask_transactions(open, Then::Quit);
        }
        true
    }

    pub(super) fn answer_transactions(
        &mut self,
        ctx: &egui::Context,
        confirm: TxConfirm,
        answer: TxAnswer,
    ) {
        match (answer, confirm.then) {
            (TxAnswer::Cancel, _) => {}
            (TxAnswer::Commit, then) => {
                // Turning auto-commit on commits first.
                let command = if then == Then::AutoCommit {
                    TxCommand::AutoCommit(true)
                } else {
                    TxCommand::Commit
                };
                for id in confirm.connections {
                    self.ending.insert(id, then);
                    self.backend.transaction(id, command);
                }
            }
            (TxAnswer::Rollback, Then::AutoCommit) => {
                for id in confirm.connections {
                    self.ending.insert(id, Then::AutoCommit);
                    self.backend.transaction(id, TxCommand::Rollback);
                }
            }
            // Closing a session rolls its transaction back.
            (TxAnswer::Rollback, Then::Disconnect(id)) => self.disconnect(id),
            (TxAnswer::Rollback, Then::Quit) => {
                self.tx_settled = true;
                self.quit(ctx);
            }
        }
    }

    /// A commit, rollback or switch finished: what waited on it follows.
    pub(super) fn transaction_done(
        &mut self,
        ctx: &egui::Context,
        id: ConnectionId,
        command: TxCommand,
        result: Result<()>,
    ) {
        let then = self.ending.remove(&id);
        if let Err(error) = result {
            let title = match command {
                TxCommand::Commit => "Could not commit",
                TxCommand::Rollback => "Could not roll back",
                TxCommand::AutoCommit(true) => "Could not switch to auto-commit",
                TxCommand::AutoCommit(false) => "Could not switch to manual commit",
            };
            self.toasts.push(ctx, Toast::error(title, error.message));
            // Whatever else waited with it waits for the user again.
            if let Some(then) = then {
                self.ending.retain(|_, other| *other != then);
            }
            return;
        }
        match then {
            None => match command {
                TxCommand::Commit => self.toasts.push(ctx, Toast::success("Committed")),
                TxCommand::Rollback => self.toasts.push(ctx, Toast::info("Rolled back")),
                TxCommand::AutoCommit(_) => {}
            },
            Some(Then::AutoCommit) => {
                if command == TxCommand::Rollback {
                    self.backend.transaction(id, TxCommand::AutoCommit(true));
                }
            }
            Some(then) if self.ending.values().all(|other| *other != then) => match then {
                Then::Disconnect(target) => self.disconnect(target),
                Then::Quit => {
                    self.tx_settled = true;
                    self.quit(ctx);
                }
                Then::AutoCommit => {}
            },
            Some(_) => {}
        }
    }
}

/// Asks about open transactions. Returns the answer once given.
pub fn confirm_transactions(ctx: &egui::Context, confirm: &TxConfirm) -> Option<TxAnswer> {
    let palette = Palette::current(ctx);
    let mut answer = None;
    let modal = egui::Modal::new(egui::Id::new("confirm-transactions"))
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
            let (title, after) = match confirm.then {
                Then::AutoCommit => ("Commit before switching to auto-commit?", "switching"),
                Then::Disconnect(_) => ("Commit before disconnecting?", "disconnecting"),
                Then::Quit => ("Commit before closing?", "closing"),
            };
            ui.label(
                RichText::new(title)
                    .font(theme::semibold(16.0))
                    .color(palette.text),
            );
            let text = match confirm.names.as_slice() {
                [name] => format!(
                    "\u{201C}{name}\u{201D} has a transaction open. Its changes are rolled back unless you commit them before {after}."
                ),
                names => format!(
                    "{} connections have a transaction open: {}. Their changes are rolled back unless you commit them before {after}.",
                    names.len(),
                    names.join(", ")
                ),
            };
            ui.add(egui::Label::new(RichText::new(text).color(palette.secondary)).wrap());
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if widgets::button(ui, Tone::Primary, Some(Icon::Check), "Commit").clicked() {
                        answer = Some(TxAnswer::Commit);
                    }
                    if widgets::button(ui, Tone::Secondary, Some(Icon::Undo), "Roll back")
                        .clicked()
                    {
                        answer = Some(TxAnswer::Rollback);
                    }
                    if widgets::button(ui, Tone::Ghost, None, "Cancel").clicked() {
                        answer = Some(TxAnswer::Cancel);
                    }
                });
            });
        });
    if modal.should_close() {
        answer = Some(TxAnswer::Cancel);
    }
    answer
}
