//! Asking a language model about queries: to write one from a
//! description, to explain the statement under the cursor, or to fix the
//! one that failed. The request goes with the connection's schema; what
//! comes back goes into the editor for review and never runs on its own.

use std::ops::Range;
use std::sync::mpsc;

use egui::{CornerRadius, RichText, Stroke};
use violet_core::{Language, Secret};
use violet_storage::KeyringApiKeys;

use super::VioletApp;
use super::action::Action;
use super::editor;
use super::icons::Icon;
use super::state::{ConnectionCatalog, Link};
use super::theme::{self, Palette};
use super::toasts::Toast;
use super::widgets::{self, Tone};
use super::workspace::{Outcome, TabId};
use crate::ai::{self, AiSettings, Answer, Provider, Task};

/// What Ask AI opens for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// A query from a description.
    Write,
    /// The selection, else the statement under the cursor, explained.
    Explain,
    /// The statement whose run failed, corrected.
    Fix,
}

/// The Ask AI dialog, for an editor tab.
pub struct AiDialog {
    tab: TabId,
    task: Task,
    /// The statement the task is about, as sent: secrets taken out.
    statement: Option<String>,
    /// Where a fix goes: the editor's range and the text it held, replaced
    /// while it still holds it.
    target: Option<(Range<usize>, String)>,
    /// Sends as soon as the schema is read and the provider is set up.
    auto: bool,
    prompt: String,
    /// The provider settings being edited, while they show.
    setup: Option<Setup>,
    /// Whether the keychain holds a key for each provider.
    keys: [bool; 2],
    state: State,
}

struct Setup {
    settings: AiSettings,
    /// A new key; empty keeps the one in the keychain.
    key: String,
}

enum State {
    Idle,
    Waiting(mpsc::Receiver<Result<Answer, String>>),
    Done(Answer),
    Failed(String),
}

/// What the dialog shows about the schema it would send.
struct SchemaStatus {
    /// The connection's name; `None` without one.
    connection: Option<String>,
    online: bool,
    described: usize,
    relations: usize,
    loading: bool,
}

fn has_key(provider: Provider) -> bool {
    KeyringApiKeys
        .get(provider.key_name())
        .is_ok_and(|key| key.is_some_and(|key| !key.is_empty()))
}

fn index(provider: Provider) -> usize {
    match provider {
        Provider::Anthropic => 0,
        Provider::OpenAi => 1,
    }
}

impl AiDialog {
    fn ready(&self, settings: &AiSettings) -> bool {
        settings.ready(self.keys[index(settings.provider)])
    }
}

impl VioletApp {
    /// Opens the dialog for an editor; on the provider settings until
    /// they are complete. Explaining and fixing send right away.
    pub(super) fn open_ai(&mut self, ctx: &egui::Context, tab_id: TabId, mode: Mode) {
        let language = self.tab_language(tab_id);
        let Some(tab) = self
            .workspace
            .get(tab_id)
            .filter(|tab| tab.data().is_none())
        else {
            return;
        };
        let selection =
            tab.selection.start.min(tab.sql.len())..tab.selection.end.min(tab.sql.len());
        let (task, statement, target) = match mode {
            Mode::Write => {
                let selected = tab.sql[selection].trim();
                let statement =
                    (!selected.is_empty()).then(|| crate::lang::redact_secrets(language, selected));
                (Task::Write, statement, None)
            }
            Mode::Explain => {
                let range = if selection.is_empty() {
                    crate::lang::statement_at(language, &tab.sql, selection.start)
                } else {
                    Some(selection)
                };
                let statement = range
                    .and_then(|range| tab.sql.get(range))
                    .map(str::trim)
                    .filter(|statement| !statement.is_empty());
                let Some(statement) = statement else {
                    self.toasts
                        .push(ctx, Toast::info("Put the cursor in a statement to explain"));
                    return;
                };
                (Task::Explain, Some(statement.to_owned()), None)
            }
            Mode::Fix => {
                let Some(Outcome::Failed { error, executed }) = &tab.outcome else {
                    return;
                };
                let original: String = executed.sql.chars().skip(executed.shift).collect();
                let range = executed.offset..executed.offset + original.len();
                let target = (tab.sql.get(range.clone()) == Some(original.as_str()))
                    .then(|| (range, original.clone()));
                // The detail stays here: servers put row values in it, such as
                // `Key (email)=(a@b.c) already exists` or `Failing row
                // contains (…)`, and no row leaves without the user's say.
                let mut why = error.message.clone();
                if let Some(hint) = &error.hint {
                    why.push_str(&format!("\nHint: {hint}"));
                }
                if let Some(code) = &error.code {
                    why.push_str(&format!("\nCode: {code}"));
                }
                if let Some(position) = error.position {
                    let at = (position as usize).saturating_sub(executed.shift);
                    why.push_str(&format!("\nAt character {at} of the statement."));
                }
                (Task::Fix(why), Some(original.trim().to_owned()), target)
            }
        };
        // Explaining or fixing sends the statement as it is: one that sets a
        // password stays here.
        if mode != Mode::Write
            && statement
                .as_deref()
                .is_some_and(|statement| crate::lang::sets_secret(language, statement))
        {
            self.toasts.push(
                ctx,
                Toast::error(
                    "Not sent to the AI",
                    "The statement sets a password or a secret.",
                ),
            );
            return;
        }
        let keys = Provider::ALL.map(has_key);
        let mut dialog = AiDialog {
            tab: tab_id,
            task,
            statement,
            target,
            auto: mode != Mode::Write,
            prompt: String::new(),
            setup: None,
            keys,
            state: State::Idle,
        };
        if !dialog.ready(&self.settings.ai) {
            dialog.setup = Some(Setup {
                settings: self.settings.ai.clone(),
                key: String::new(),
            });
        }
        self.ai_dialog = Some(dialog);
    }

    /// The name of a connection's driver, such as `PostgreSQL`.
    pub(super) fn driver_name(&self, id: violet_core::ConnectionId) -> String {
        self.profiles
            .get(self.profile_of(id))
            .and_then(|profile| self.backend.registry().get(&profile.driver))
            .map_or_else(
                || "a SQL database".to_owned(),
                |driver| driver.info().name.clone(),
            )
    }

    /// The schemas the model is told about, as described from the tree.
    fn ai_schema(&self, tab: TabId) -> (Option<violet_core::ConnectionId>, ai::Schema) {
        let Some(id) = self.workspace.get(tab).and_then(|tab| tab.connection) else {
            return (None, ai::Schema::default());
        };
        let catalog = ConnectionCatalog {
            explorer: &self.explorer,
            id: Some(id),
        };
        let schemas = ai::schemas_to_describe(
            self.search_path(id),
            crate::completion::Catalog::schemas(&catalog),
        );
        let foreign_keys = self.language(id) == Language::Sql;
        (Some(id), ai::describe(&catalog, &schemas, foreign_keys))
    }

    pub(super) fn ai_modal(&mut self, ctx: &egui::Context) {
        let Some(tab) = self.ai_dialog.as_ref().map(|dialog| dialog.tab) else {
            return;
        };
        if self.workspace.get(tab).is_none() {
            self.ai_dialog = None;
            return;
        }
        let (connection, schema) = self.ai_schema(tab);
        let online = connection.is_some_and(|id| self.explorer.link(id).is_online());
        if let Some(id) = connection
            && online
            && !schema.needs.is_empty()
        {
            self.actions
                .push(Action::LoadForCompletion(id, schema.needs.clone()));
        }
        let status = SchemaStatus {
            connection: connection.map(|id| self.profile_name(id)),
            online,
            described: schema.described,
            relations: schema.relations,
            loading: online && !schema.needs.is_empty(),
        };
        let language = self.tab_language(tab);
        let Some(dialog) = &mut self.ai_dialog else {
            return;
        };
        if dialog.auto
            && matches!(dialog.state, State::Idle)
            && dialog.ready(&self.settings.ai)
            && !status.loading
        {
            dialog.auto = false;
            self.actions.push(Action::AiGenerate);
        }
        if let State::Waiting(receiver) = &dialog.state {
            match receiver.try_recv() {
                Ok(Ok(answer)) => dialog.state = State::Done(answer),
                Ok(Err(error)) => dialog.state = State::Failed(error),
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    dialog.state = State::Failed("The request stopped.".to_owned());
                }
            }
        }
        if !show(
            ctx,
            dialog,
            &self.settings.ai,
            &status,
            language,
            &mut self.actions,
        ) {
            self.ai_dialog = None;
        }
    }

    /// Sends the dialog's request, off the interface thread.
    pub(super) fn ai_generate(&mut self, ctx: &egui::Context) {
        let Some(tab_id) = self.ai_dialog.as_ref().map(|dialog| dialog.tab) else {
            return;
        };
        let (connection, schema) = self.ai_schema(tab_id);
        let language = self.tab_language(tab_id);
        let product = connection.map_or_else(
            || "a SQL database".to_owned(),
            |id| match self.explorer.link(id) {
                Link::Online(server) => server.version.clone(),
                _ => self.driver_name(id),
            },
        );
        let Some(dialog) = &mut self.ai_dialog else {
            return;
        };
        let prompt = dialog.prompt.trim();
        if (dialog.task == Task::Write && prompt.is_empty())
            || !dialog.ready(&self.settings.ai)
            || matches!(dialog.state, State::Waiting(_))
        {
            return;
        }
        let request = ai::Request {
            product,
            language: match language {
                Language::Sql => "SQL",
                Language::Redis => "Redis commands",
                Language::Mongodb => "MongoDB queries",
            }
            .to_owned(),
            schema: schema.text,
            task: dialog.task.clone(),
            statement: dialog.statement.clone(),
            prompt: prompt.to_owned(),
        };
        let settings = self.settings.ai.clone();
        let (sender, receiver) = mpsc::channel();
        let repaint = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("ai".to_owned())
            .spawn(move || {
                let answer = match KeyringApiKeys.get(settings.provider.key_name()) {
                    Ok(key) => ai::ask(&settings, key.as_ref(), &request),
                    Err(error) => Err(error.to_string()),
                };
                let _ = sender.send(answer);
                repaint.request_repaint();
            });
        dialog.state = match spawned {
            Ok(_) => State::Waiting(receiver),
            Err(error) => State::Failed(error.to_string()),
        };
    }

    /// Puts the answer in its editor, selected: over the statement it
    /// fixes while that is unchanged, else in place of the selection or on
    /// lines of its own at the cursor. Then closes the dialog.
    pub(super) fn ai_insert(&mut self, ctx: &egui::Context) {
        let Some(dialog) = self.ai_dialog.take() else {
            return;
        };
        let State::Done(answer) = dialog.state else {
            self.ai_dialog = Some(dialog);
            return;
        };
        let language = self.tab_language(dialog.tab);
        if self.workspace.get(dialog.tab).is_some() {
            self.workspace.active = Some(dialog.tab);
        }
        let Some(tab) = self.workspace.get_mut(dialog.tab) else {
            return;
        };
        if let Some((range, original)) = dialog.target
            && tab.sql.get(range.clone()) == Some(original.as_str())
        {
            let mut block = answer.sql.trim().to_owned();
            if language == Language::Sql {
                block = ai::keep_leading_comments(&original, &block);
            }
            if original.trim_end().ends_with(';') && !block.ends_with(';') {
                block.push(';');
            }
            let text = format!(
                "{}{block}{}",
                &tab.sql[..range.start],
                &tab.sql[range.end..]
            );
            let selection = range.start..range.start + block.len();
            editor::replace_text(ctx, tab, crate::editing::Edited { text, selection });
            return;
        }
        let range = tab.selection.start.min(tab.sql.len())..tab.selection.end.min(tab.sql.len());
        let mut block = answer.sql.trim().to_owned();
        if language == Language::Sql && !block.ends_with(';') {
            block.push(';');
        }
        let mut start = range.start;
        let mut text = tab.sql[..range.start].to_owned();
        if range.is_empty() && !text.is_empty() && !text.ends_with('\n') {
            text.push_str("\n\n");
            start = text.len();
        }
        text.push_str(&block);
        let end = text.len();
        if range.is_empty() && !tab.sql[range.end..].starts_with('\n') {
            text.push('\n');
        }
        text.push_str(&tab.sql[range.end..]);
        editor::replace_text(
            ctx,
            tab,
            crate::editing::Edited {
                text,
                selection: start..end,
            },
        );
    }

    /// Keeps the provider settings, and the key in the keychain.
    pub(super) fn save_ai_settings(
        &mut self,
        ctx: &egui::Context,
        settings: AiSettings,
        key: Option<Secret>,
    ) {
        let provider = settings.provider;
        if let Some(key) = key.filter(|key| !key.is_empty())
            && let Err(error) = KeyringApiKeys.set(provider.key_name(), &key)
        {
            self.toasts.push(
                ctx,
                Toast::error("The API key was not saved", error.to_string()),
            );
            return;
        }
        self.settings.ai = settings;
        self.save_settings(ctx);
        if let Some(dialog) = &mut self.ai_dialog {
            dialog.keys[index(provider)] = has_key(provider);
            if dialog.ready(&self.settings.ai) {
                dialog.setup = None;
            }
        }
    }

    /// Takes a provider's key out of the keychain.
    pub(super) fn forget_ai_key(&mut self, ctx: &egui::Context, provider: Provider) {
        if let Err(error) = KeyringApiKeys.delete(provider.key_name()) {
            self.toasts.push(
                ctx,
                Toast::error("The API key was not removed", error.to_string()),
            );
        }
        if let Some(dialog) = &mut self.ai_dialog {
            dialog.keys[index(provider)] = has_key(provider);
        }
    }
}

#[cfg(debug_assertions)]
impl VioletApp {
    /// Types the request, for the `ai` scene.
    pub(super) fn ai_scene_prompt(&mut self, prompt: &str) {
        if let Some(dialog) = &mut self.ai_dialog {
            prompt.clone_into(&mut dialog.prompt);
        }
    }

    /// Whether the answer, or an error, came back.
    pub(super) fn ai_scene_settled(&self) -> bool {
        self.ai_dialog
            .as_ref()
            .is_some_and(|dialog| matches!(dialog.state, State::Done(_) | State::Failed(_)))
    }
}

/// Shows the dialog. Returns whether it is still open.
fn show(
    ctx: &egui::Context,
    dialog: &mut AiDialog,
    settings: &AiSettings,
    status: &SchemaStatus,
    language: Language,
    actions: &mut Vec<Action>,
) -> bool {
    let palette = Palette::current(ctx);
    let mut open = true;
    let modal = egui::Modal::new(egui::Id::new("ask-ai"))
        .frame(super::edit_flow::frame(&palette))
        .show(ctx, |ui| {
            ui.set_width(560.0);
            ui.spacing_mut().item_spacing.y = 10.0;
            ui.horizontal(|ui| {
                let title = match dialog.task {
                    Task::Write => "Ask AI",
                    Task::Explain => "Explain the statement",
                    Task::Fix(_) => "Fix the statement",
                };
                ui.label(
                    RichText::new(title)
                        .font(theme::semibold(16.0))
                        .color(palette.text),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let tip = if dialog.setup.is_some() {
                        "Hide provider settings"
                    } else {
                        "Provider settings"
                    };
                    if widgets::icon_button(ui, Icon::Settings, tip).clicked() {
                        dialog.setup = match dialog.setup {
                            Some(_) => None,
                            None => Some(Setup {
                                settings: settings.clone(),
                                key: String::new(),
                            }),
                        };
                    }
                    ui.label(
                        RichText::new(format!(
                            "{} · {}",
                            settings.provider.label(),
                            settings.model()
                        ))
                        .color(palette.dim),
                    );
                });
            });
            if let Some(setup) = &mut dialog.setup {
                setup_form(ui, &palette, setup, dialog.keys, actions);
                ui.separator();
            }
            ui.label(RichText::new(sent_note(status, &dialog.task)).color(palette.secondary));
            if dialog.task != Task::Write
                && let Some(statement) = &dialog.statement
            {
                code_block(ui, &palette, statement, 120.0);
            }
            let waiting = matches!(dialog.state, State::Waiting(_));
            let (hint, rows) = match dialog.task {
                Task::Write => (
                    "Describe the query: customers who ordered nothing this year…",
                    3,
                ),
                Task::Explain => ("Anything in particular to ask about it? (optional)", 1),
                Task::Fix(_) => ("Anything the fix should keep in mind? (optional)", 1),
            };
            let response = ui.add_enabled(
                !waiting,
                egui::TextEdit::multiline(&mut dialog.prompt)
                    .hint_text(hint)
                    .desired_rows(rows)
                    .desired_width(f32::INFINITY)
                    .margin(egui::vec2(10.0, 8.0)),
            );
            if dialog.setup.is_none() && matches!(dialog.state, State::Idle) {
                response.request_focus();
            }
            let submit = response.has_focus()
                && ui.input(|input| input.modifiers.command && input.key_pressed(egui::Key::Enter));
            let ready = dialog.ready(settings);
            let can_send = ready
                && !waiting
                && (dialog.task != Task::Write || !dialog.prompt.trim().is_empty());
            match &dialog.state {
                State::Idle => {}
                State::Waiting(_) => {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(14.0).color(palette.accent));
                        let doing = match dialog.task {
                            Task::Write => "Writing the query…",
                            Task::Explain => "Reading the statement…",
                            Task::Fix(_) => "Fixing the statement…",
                        };
                        ui.label(RichText::new(doing).color(palette.dim));
                    });
                }
                State::Failed(error) => {
                    ui.label(RichText::new(error).color(palette.danger));
                }
                State::Done(answer) => answer_view(ui, &palette, answer, language),
            }
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let has_sql =
                        matches!(&dialog.state, State::Done(answer) if !answer.sql.is_empty());
                    let insert = if dialog.target.is_some() {
                        "Replace the statement"
                    } else {
                        "Insert in editor"
                    };
                    if has_sql
                        && widgets::button(ui, Tone::Primary, Some(Icon::Check), insert).clicked()
                    {
                        actions.push(Action::AiInsert);
                    }
                    let (tone, label) = if matches!(dialog.state, State::Done(_) | State::Failed(_))
                    {
                        (Tone::Secondary, "Try again")
                    } else {
                        let label = match dialog.task {
                            Task::Write => "Generate",
                            Task::Explain => "Explain",
                            Task::Fix(_) => "Fix",
                        };
                        (Tone::Primary, label)
                    };
                    let shortcut = ui.ctx().format_shortcut(&egui::KeyboardShortcut::new(
                        egui::Modifiers::COMMAND,
                        egui::Key::Enter,
                    ));
                    let generate = ui
                        .add_enabled_ui(can_send, |ui| {
                            widgets::button(ui, tone, Some(Icon::Sparkles), label)
                        })
                        .inner
                        .on_hover_text(format!("Send the request ({shortcut})"));
                    if generate.clicked() || (submit && can_send) {
                        actions.push(Action::AiGenerate);
                    }
                    if widgets::button(ui, Tone::Ghost, None, "Close").clicked() {
                        open = false;
                    }
                    if !ready {
                        ui.label(
                            RichText::new("Finish the provider settings first.").color(palette.dim),
                        );
                    }
                });
            });
        });
    open && !modal.should_close()
}

/// What goes to the provider, in a sentence.
fn sent_note(status: &SchemaStatus, task: &Task) -> String {
    let request = match task {
        Task::Write => "your request and the selected statement if any",
        Task::Explain => "the statement",
        Task::Fix(_) => "the statement and its error",
    };
    let Some(connection) = &status.connection else {
        return format!("No connection: only {request} is sent.");
    };
    if !status.online {
        return format!("{connection} is not connected: only {request} is sent.");
    }
    let schema = if status.loading {
        format!("reading the schema… {} relations so far", status.described)
    } else if status.described < status.relations {
        format!(
            "{} of {} relations described, the rest by name",
            status.described, status.relations
        )
    } else {
        format!("{} relations described", status.described)
    };
    format!(
        "Sends {request}, with the names and types of {connection}'s tables and columns \
         ({schema}); never rows. Nothing it writes runs on its own."
    )
}

fn setup_form(
    ui: &mut egui::Ui,
    palette: &Palette,
    setup: &mut Setup,
    keys: [bool; 2],
    actions: &mut Vec<Action>,
) {
    widgets::segmented(
        ui,
        &mut setup.settings.provider,
        &Provider::ALL.map(|provider| (provider, provider.label())),
    );
    let provider = setup.settings.provider;
    match provider {
        Provider::Anthropic => {
            widgets::field(ui, "Model", |ui| {
                ui.add(widgets::text_input(
                    &mut setup.settings.anthropic_model,
                    "claude-opus-5-5",
                ))
            });
        }
        Provider::OpenAi => {
            widgets::field(ui, "API address", |ui| {
                ui.add(widgets::text_input(
                    &mut setup.settings.openai_url,
                    "https://api.openai.com/v1, or http://localhost:11434/v1 for Ollama",
                ))
            });
            widgets::field(ui, "Model", |ui| {
                ui.add(widgets::text_input(
                    &mut setup.settings.openai_model,
                    "Model name",
                ))
            });
        }
    }
    let kept = keys[index(provider)];
    let hint = match (kept, provider) {
        (true, _) => "Saved in the keychain; type to replace it",
        (false, Provider::Anthropic) => "sk-ant-…",
        (false, Provider::OpenAi) => "Optional for local servers",
    };
    widgets::field(ui, "API key", |ui| {
        ui.add(widgets::text_input(&mut setup.key, hint).password(true))
    });
    ui.horizontal(|ui| {
        ui.label(RichText::new("Keys stay in the system keychain.").color(palette.dim));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if widgets::button(ui, Tone::Secondary, Some(Icon::Save), "Save settings").clicked() {
                let key = std::mem::take(&mut setup.key);
                actions.push(Action::SaveAiSettings(
                    setup.settings.clone(),
                    (!key.is_empty()).then(|| Secret::new(key)),
                ));
            }
            if kept && widgets::button(ui, Tone::Ghost, Some(Icon::Trash), "Forget key").clicked() {
                actions.push(Action::ForgetAiKey(provider));
            }
        });
    });
}

fn answer_view(ui: &mut egui::Ui, palette: &Palette, answer: &Answer, language: Language) {
    if !answer.sql.is_empty() {
        if language == Language::Sql && !crate::sql::plan(&answer.sql).reads_only {
            widgets::pill(
                ui,
                "Changes data or structure",
                palette.warning,
                palette.warning.gamma_multiply(0.15),
            );
        }
        code_block(ui, palette, &answer.sql, 260.0);
    }
    if !answer.notes.is_empty() {
        egui::ScrollArea::vertical()
            .id_salt("ai-notes")
            .max_height(280.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                let color = if answer.sql.is_empty() {
                    palette.text
                } else {
                    palette.secondary
                };
                ui.add(
                    egui::Label::new(RichText::new(&answer.notes).color(color)).selectable(true),
                );
            });
    }
}

/// Code in a mono frame, scrolling past `height`.
fn code_block(ui: &mut egui::Ui, palette: &Palette, code: &str, height: f32) {
    egui::Frame::new()
        .fill(palette.panel)
        .stroke(Stroke::new(1.0, palette.outline))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(egui::Margin::same(10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::ScrollArea::vertical()
                .id_salt(("ai-code", height.to_bits()))
                .max_height(height)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui.add(
                        egui::Label::new(
                            RichText::new(code)
                                .font(theme::mono(12.5))
                                .color(palette.text),
                        )
                        .selectable(true),
                    );
                });
        });
}
