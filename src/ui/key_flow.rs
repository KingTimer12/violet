//! A key-value database's keys in the tree: listing them a page at a time,
//! and changing one from its menu (TTL, name, delete).

use egui::{CornerRadius, RichText, Stroke};
use violet_app::{CachePolicy, ExecOptions};
use violet_core::{ConnectionId, QueryOutput, Result};
use violet_lang::redis::actions;

use super::VioletApp;
use super::action::Action;
use super::icons::Icon;
use super::state::{Link, NodeKey};
use super::theme::{self, Palette};
use super::toasts::Toast;
use super::widgets::{self, Tone};

/// What the key's menu asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyOp {
    Ttl,
    Rename,
    Delete,
}

/// The dialog changing one key.
pub struct KeyDialog {
    pub connection: ConnectionId,
    pub schema: String,
    pub key: String,
    pub op: KeyOp,
    /// Seconds to live, or the new name.
    pub text: String,
    pub error: Option<String>,
}

/// Commands run for the tree, waiting for their answer.
pub struct Pending {
    pub connection: ConnectionId,
    pub schema: String,
    pub done: String,
}

impl KeyDialog {
    /// The commands the dialog's answer runs, or why it cannot.
    fn script(&self) -> std::result::Result<String, String> {
        match self.op {
            KeyOp::Delete => Ok(actions::delete(&[self.key.as_str()])),
            KeyOp::Rename => {
                let to = self.text.trim();
                if to.is_empty() || to == self.key {
                    return Err("Type the key's new name".into());
                }
                Ok(actions::rename(&self.key, to))
            }
            KeyOp::Ttl => {
                let text = self.text.trim();
                if text.is_empty() {
                    return Ok(actions::expire(&self.key, None));
                }
                match text.parse::<u64>() {
                    Ok(0) | Err(_) => Err(
                        "Type a number of seconds, or leave it empty to keep the key for ever"
                            .into(),
                    ),
                    Ok(seconds) => Ok(actions::expire(&self.key, Some(seconds))),
                }
            }
        }
    }

    fn done(&self) -> String {
        match self.op {
            KeyOp::Delete => format!("Deleted {}", self.key),
            KeyOp::Rename => format!("Renamed {} to {}", self.key, self.text.trim()),
            KeyOp::Ttl if self.text.trim().is_empty() => format!("{} no longer expires", self.key),
            KeyOp::Ttl => format!("{} expires in {} s", self.key, self.text.trim()),
        }
    }
}

impl VioletApp {
    /// Lists `schema`'s keys again from the first page, or the next page.
    pub(super) fn load_keys(&mut self, connection: ConnectionId, schema: String, more: bool) {
        let keys = self
            .explorer
            .keys
            .entry((connection, schema.clone()))
            .or_default();
        if keys.loading || (more && keys.next.is_none()) {
            return;
        }
        keys.loading = true;
        keys.error = None;
        let glob = if more {
            keys.applied.clone()
        } else {
            keys.glob()
        };
        let cursor = if more { keys.next.clone() } else { None };
        self.backend.load_entries(connection, schema, glob, cursor);
    }

    /// A key-value store's databases arrived: the session's own opens, as
    /// do those open before, listing their keys.
    pub(super) fn keyspaces_listed(
        &mut self,
        connection: ConnectionId,
        result: &Result<Vec<String>>,
    ) {
        let Ok(schemas) = result else {
            return;
        };
        let any_open = self
            .explorer
            .expanded
            .iter()
            .any(|key| matches!(key, NodeKey::Schema(owner, _) if *owner == connection));
        if !any_open && let Link::Online(server) = self.explorer.link(connection) {
            let own = NodeKey::Schema(connection, server.database.clone());
            self.explorer.expanded.insert(own);
        }
        for schema in schemas {
            if self
                .explorer
                .is_expanded(&NodeKey::Schema(connection, schema.clone()))
            {
                self.load_keys(connection, schema.clone(), false);
            }
        }
    }

    /// A page of keys arrived.
    pub(super) fn keys_listed(
        &mut self,
        connection: ConnectionId,
        schema: String,
        pattern: String,
        more: bool,
        result: Result<violet_core::EntryPage>,
    ) {
        let keys = self.explorer.keys.entry((connection, schema)).or_default();
        keys.loading = false;
        keys.listed = true;
        match result {
            Ok(page) => {
                let mut list: Vec<_> = if more { keys.list.to_vec() } else { Vec::new() };
                // A SCAN may hand a key out twice.
                let seen: std::collections::HashSet<String> =
                    list.iter().map(|object| object.name.clone()).collect();
                list.extend(page.objects.into_iter().filter(|o| !seen.contains(&o.name)));
                list.sort_by(|a, b| a.name.cmp(&b.name));
                keys.list = list.into();
                keys.next = page.next;
                keys.applied = pattern;
            }
            Err(error) => keys.error = Some(error.message),
        }
    }

    pub(super) fn open_key_dialog(
        &mut self,
        connection: ConnectionId,
        schema: String,
        key: String,
        op: KeyOp,
    ) {
        let text = match op {
            KeyOp::Rename => key.clone(),
            KeyOp::Ttl | KeyOp::Delete => String::new(),
        };
        self.key_dialog = Some(KeyDialog {
            connection,
            schema,
            key,
            op,
            text,
            error: None,
        });
    }

    pub(super) fn run_in_schema(
        &mut self,
        ctx: &egui::Context,
        connection: ConnectionId,
        schema: String,
        script: String,
        done: String,
    ) {
        if !self.backend.is_connected(connection) {
            self.toasts.push(ctx, Toast::info("Connect first"));
            return;
        }
        let request = self.backend.execute_in(
            connection,
            Some(schema.clone()),
            script,
            ExecOptions {
                cache: CachePolicy::Off,
                page: None,
            },
        );
        self.key_requests.insert(
            request,
            Pending {
                connection,
                schema,
                done,
            },
        );
    }

    pub(super) fn key_command_done(
        &mut self,
        ctx: &egui::Context,
        pending: Pending,
        result: &Result<QueryOutput>,
    ) {
        match result {
            Ok(output) => {
                // A rename onto an existing key answers 0 rather than fail.
                let refused = output.sets.iter().any(|set| {
                    set.columns.first().is_some_and(|c| c.name == "integer")
                        && set.rows.get(0, 0).flatten() == Some("0")
                });
                if refused {
                    self.toasts.push(
                        ctx,
                        Toast::error(
                            "Nothing changed",
                            "The key does not exist any more, had no expiry to remove, or \
                             the new name is taken.",
                        ),
                    );
                } else {
                    self.toasts.push(ctx, Toast::success(pending.done));
                }
            }
            Err(error) => self.toasts.push(
                ctx,
                Toast::error("The key could not be changed", error.message.clone()),
            ),
        }
        // Open data tabs of the key read it again.
        let tabs: Vec<_> = self
            .workspace
            .tabs
            .iter()
            .filter(|tab| {
                tab.connection == Some(pending.connection)
                    && tab.data().is_some_and(|view| view.schema == pending.schema)
                    && tab.edits.is_empty()
            })
            .map(|tab| tab.id)
            .collect();
        for tab in tabs {
            self.run_data(ctx, tab, CachePolicy::Refresh);
        }
        self.load_keys(pending.connection, pending.schema, false);
    }
}

/// Shows the dialog. Returns whether it is still open.
pub fn show(ctx: &egui::Context, dialog: &mut KeyDialog, actions: &mut Vec<Action>) -> bool {
    let palette = Palette::current(ctx);
    let mut open = true;
    let (title, body, icon, label, tone) = match dialog.op {
        KeyOp::Ttl => (
            "Time to live",
            "Seconds until the key expires. Leave it empty to keep it for ever.",
            Icon::Clock,
            "Set TTL",
            Tone::Primary,
        ),
        KeyOp::Rename => (
            "Rename key",
            "The new name. Violet will not overwrite a key that already has it.",
            Icon::Pencil,
            "Rename",
            Tone::Primary,
        ),
        KeyOp::Delete => (
            "Delete key?",
            "The key and its value are removed from the server. This cannot be undone.",
            Icon::Trash,
            "Delete",
            Tone::Danger,
        ),
    };
    let modal = egui::Modal::new(egui::Id::new("key-dialog"))
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
                RichText::new(title)
                    .font(theme::semibold(16.0))
                    .color(palette.text),
            );
            ui.label(
                RichText::new(format!("{} · {}", dialog.schema, dialog.key))
                    .font(theme::mono(12.5))
                    .color(palette.secondary),
            );
            ui.add(egui::Label::new(RichText::new(body).color(palette.secondary)).wrap());
            let mut submit = false;
            if dialog.op != KeyOp::Delete {
                let hint = if dialog.op == KeyOp::Ttl {
                    "3600"
                } else {
                    "new:name"
                };
                let response = ui.add(widgets::text_input(&mut dialog.text, hint));
                if !response.has_focus() && !response.lost_focus() && dialog.error.is_none() {
                    response.request_focus();
                }
                submit =
                    response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
            }
            if let Some(error) = &dialog.error {
                ui.label(RichText::new(error).color(palette.danger));
            }
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if widgets::button(ui, tone, Some(icon), label).clicked() || submit {
                        match dialog.script() {
                            Ok(script) => {
                                actions.push(Action::RunInSchema {
                                    connection: dialog.connection,
                                    schema: dialog.schema.clone(),
                                    script,
                                    done: dialog.done(),
                                });
                                open = false;
                            }
                            Err(error) => dialog.error = Some(error),
                        }
                    }
                    if widgets::button(ui, Tone::Ghost, None, "Cancel").clicked() {
                        open = false;
                    }
                });
            });
        });
    open && !modal.should_close()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dialog(op: KeyOp, text: &str) -> KeyDialog {
        KeyDialog {
            connection: ConnectionId::new(),
            schema: "db0".into(),
            key: "user:1".into(),
            op,
            text: text.into(),
            error: None,
        }
    }

    #[test]
    fn answers_become_commands() {
        assert_eq!(
            dialog(KeyOp::Ttl, " 60 ").script().as_deref(),
            Ok("EXPIRE user:1 60")
        );
        assert_eq!(
            dialog(KeyOp::Ttl, "").script().as_deref(),
            Ok("PERSIST user:1")
        );
        assert!(dialog(KeyOp::Ttl, "soon").script().is_err());
        assert_eq!(
            dialog(KeyOp::Rename, "user 2").script().as_deref(),
            Ok("RENAMENX user:1 \"user 2\"")
        );
        assert!(dialog(KeyOp::Rename, "user:1").script().is_err());
        assert_eq!(
            dialog(KeyOp::Delete, "").script().as_deref(),
            Ok("UNLINK user:1")
        );
    }
}
