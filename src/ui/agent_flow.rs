//! The agent panel: a coding agent such as Claude Code beside the editor,
//! spoken to over ACP (`crate::agent`), that writes queries and nothing
//! else. It gets no files, no shell and no way to run a statement: Claude
//! Code starts without its built-in tools, every permission it asks for but
//! Violet's own read-only tools is refused, and what it learns of the
//! databases comes from those MCP tools (`crate::mcp`), which read the
//! catalog and the editor, never rows.

use std::path::PathBuf;

use egui::{CornerRadius, RichText, Stroke, Ui};
use serde_json::{Value, json};
use violet_core::Language;

use super::VioletApp;
use super::action::Action;
use super::icons::Icon;
use super::mcp_flow::Pending;
use super::state::{ConnectionCatalog, Link};
use super::theme::{self, Palette};
use super::widgets::{self, Tone};
use crate::agent::{self, Agent, Incoming, RpcError};

/// The panel, open or not, and its conversation.
#[derive(Default)]
pub struct AgentPanel {
    pub open: bool,
    conn: Option<Conn>,
    input: String,
    entries: Vec<Entry>,
    /// Leave the editor and schema out of the next messages.
    no_context: bool,
    /// The command field, while the settings show.
    setup: Option<String>,
    /// Violet's MCP server, started with the first agent.
    mcp: Option<crate::mcp::Server>,
    /// Tool calls waiting for the catalog.
    pending: Vec<Pending>,
}

struct Conn {
    agent: Agent,
    phase: Phase,
    /// The `session/prompt` waiting for its end.
    prompt: Option<u64>,
    /// Whether the session has Violet's MCP tools.
    tools: bool,
}

/// What Claude Code is told it is, in place of its coding prompt.
const SYSTEM_PROMPT: &str = "You are the query assistant inside Violet, a database client. \
You write SQL (or the commands of the connection's database) for what the user asks, and \
nothing else: you cannot run statements, read rows, or touch files, and must not try. \
Use the violet tools to learn the user's connections, schemas, tables and columns, and the \
editor they are in, before writing; use only names that exist there. Answer with the \
statement in a fenced ```sql block, then a sentence or two on what it does and anything to \
check before running it, such as a write without a WHERE. The user inserts it in their editor \
and runs it themselves. Reply in the language the user writes in.";

/// Violet's MCP tools as Claude Code names them: `mcp__violet__<tool>`.
fn violet_tools() -> Vec<String> {
    crate::mcp::tools()
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tool| tool.get("name")?.as_str())
        .map(|name| format!("mcp__violet__{name}"))
        .collect()
}

/// Whether a permission request is for one of Violet's own tools, which
/// only read the catalog and the editor.
fn asks_for_violet_tool(params: &Value) -> bool {
    let named = params
        .pointer("/toolCall/_meta/claudeCode/toolName")
        .or_else(|| params.pointer("/toolCall/title"))
        .and_then(Value::as_str);
    named.is_some_and(|name| violet_tools().iter().any(|tool| tool == name))
}

/// The answer that allows a permission request once, when it offers that.
fn allowance(params: &Value) -> Option<Value> {
    let option = params
        .get("options")?
        .as_array()?
        .iter()
        .find(|option| option.get("kind").and_then(Value::as_str) == Some("allow_once"))?
        .get("optionId")?
        .clone();
    Some(json!({"outcome": {"outcome": "selected", "optionId": option}}))
}

/// The session options that keep Claude Code to writing queries: none of
/// its built-in tools (shell, files, web, subagents), none of the user's
/// own settings, hooks or MCP servers, and the prompt above. Agents other
/// than Claude Code ignore them.
fn session_meta() -> Value {
    json!({
        "systemPrompt": SYSTEM_PROMPT,
        "claudeCode": {
            "options": {
                "tools": [],
                "settingSources": [],
                "strictMcpConfig": true,
                // Violet's own tools only read: no asking for each call.
                "allowedTools": violet_tools()
            }
        }
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Phase {
    /// Waiting for `initialize`'s answer, by request id.
    Starting(u64),
    /// Waiting for `session/new`'s answer.
    Opening(u64),
    Ready(String),
}

#[derive(Debug)]
enum Entry {
    User(String),
    Agent { id: Option<String>, text: String },
    Thought(String),
    Tool(Tool),
    Plan(Vec<(String, String)>),
    Notice { text: String, error: bool },
}

#[derive(Debug, Default)]
struct Tool {
    id: String,
    title: String,
    status: String,
    text: String,
    /// Files changed: path, then the new text's line count.
    diffs: Vec<(String, usize)>,
}

/// What the panel shows of the agent's state.
fn status(conn: Option<&Conn>) -> (&'static str, bool) {
    match conn.map(|conn| (&conn.phase, conn.prompt.is_some())) {
        None => ("Stopped", false),
        Some((Phase::Starting(_) | Phase::Opening(_), _)) => ("Starting…", true),
        Some((Phase::Ready(_), true)) => ("Working…", true),
        Some((Phase::Ready(_), false)) => ("Ready", false),
    }
}

impl AgentPanel {
    fn notice(&mut self, text: impl Into<String>, error: bool) {
        self.entries.push(Entry::Notice {
            text: text.into(),
            error,
        });
    }

    /// Takes in a `session/update`.
    fn update(&mut self, update: &Value) {
        let text_of = |update: &Value| {
            update
                .pointer("/content/text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        match update.get("sessionUpdate").and_then(Value::as_str) {
            Some("agent_message_chunk") => {
                let id = update
                    .get("messageId")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let chunk = text_of(update);
                match self.entries.last_mut() {
                    Some(Entry::Agent { id: last, text }) if id.is_none() || *last == id => {
                        text.push_str(&chunk);
                    }
                    _ => self.entries.push(Entry::Agent { id, text: chunk }),
                }
            }
            Some("agent_thought_chunk") => {
                let chunk = text_of(update);
                match self.entries.last_mut() {
                    Some(Entry::Thought(text)) => text.push_str(&chunk),
                    _ => self.entries.push(Entry::Thought(chunk)),
                }
            }
            Some("tool_call" | "tool_call_update") => {
                let Some(id) = update.get("toolCallId").and_then(Value::as_str) else {
                    return;
                };
                let index = self
                    .entries
                    .iter()
                    .position(|entry| matches!(entry, Entry::Tool(tool) if tool.id == id));
                let index = index.unwrap_or_else(|| {
                    self.entries.push(Entry::Tool(Tool {
                        id: id.to_owned(),
                        status: "pending".to_owned(),
                        ..Tool::default()
                    }));
                    self.entries.len() - 1
                });
                let Entry::Tool(tool) = &mut self.entries[index] else {
                    return;
                };
                apply_tool(tool, update);
            }
            Some("plan") => {
                let entries = update
                    .get("entries")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|entry| {
                        let field = |name: &str| {
                            entry
                                .get(name)
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned()
                        };
                        (field("content"), field("status"))
                    })
                    .collect();
                // One plan, kept up to date where it first showed.
                match self
                    .entries
                    .iter_mut()
                    .rev()
                    .find(|entry| matches!(entry, Entry::Plan(_)))
                {
                    Some(Entry::Plan(plan)) => *plan = entries,
                    _ => self.entries.push(Entry::Plan(entries)),
                }
            }
            _ => {}
        }
    }
}

/// Puts a tool call's report on `tool`: only the fields it carries.
fn apply_tool(tool: &mut Tool, update: &Value) {
    if let Some(title) = update.get("title").and_then(Value::as_str) {
        title.clone_into(&mut tool.title);
    }
    if let Some(status) = update.get("status").and_then(Value::as_str) {
        status.clone_into(&mut tool.status);
    }
    let Some(content) = update.get("content").and_then(Value::as_array) else {
        return;
    };
    tool.text.clear();
    tool.diffs.clear();
    for item in content {
        match item.get("type").and_then(Value::as_str) {
            Some("diff") => {
                let path = item.get("path").and_then(Value::as_str).unwrap_or_default();
                let lines = item
                    .get("newText")
                    .and_then(Value::as_str)
                    .map_or(0, |text| text.lines().count());
                tool.diffs.push((path.to_owned(), lines));
            }
            Some("content") => {
                if let Some(text) = item.pointer("/content/text").and_then(Value::as_str) {
                    if !tool.text.is_empty() {
                        tool.text.push('\n');
                    }
                    tool.text.push_str(text);
                }
            }
            _ => {}
        }
    }
}

impl VioletApp {
    /// Where the agent works: the open workspace's scripts.
    fn agent_cwd(&self) -> PathBuf {
        self.scripts.root().to_path_buf()
    }

    pub(super) fn toggle_agent(&mut self, ctx: &egui::Context) {
        self.agent.open = !self.agent.open;
        if self.agent.open && self.agent.conn.is_none() {
            self.start_agent(ctx);
        }
    }

    /// Starts the agent, ending the one running.
    pub(super) fn start_agent(&mut self, ctx: &egui::Context) {
        self.agent.conn = None;
        let cwd = self.agent_cwd();
        if let Err(error) = std::fs::create_dir_all(&cwd) {
            self.agent.notice(
                format!("The scripts folder could not be made: {error}"),
                true,
            );
            return;
        }
        let command = self.settings.agent_command.trim();
        let command = if command.is_empty() {
            agent::DEFAULT_COMMAND
        } else {
            command
        };
        if self.agent.mcp.is_none() {
            let wake = ctx.clone();
            match crate::mcp::Server::start(move || wake.request_repaint()) {
                Ok(server) => self.agent.mcp = Some(server),
                Err(error) => self.agent.notice(
                    format!("Violet's tools could not start, so the agent will not see the schema: {error}"),
                    true,
                ),
            }
        }
        let wake = ctx.clone();
        match Agent::spawn(command, &cwd, move || wake.request_repaint()) {
            Ok(agent) => {
                let id = agent.request("initialize", agent::initialize_params());
                self.agent.conn = Some(Conn {
                    agent,
                    phase: Phase::Starting(id),
                    prompt: None,
                    tools: false,
                });
            }
            Err(error) => self
                .agent
                .notice(format!("`{command}` could not start: {error}"), true),
        }
    }

    pub(super) fn stop_agent(&mut self) {
        if self.agent.conn.take().is_some() {
            self.agent.notice("The agent was stopped.", false);
        }
    }

    /// Takes in what the agent sent since the last frame.
    pub(super) fn poll_agent(&mut self, ctx: &egui::Context) {
        if let Some(server) = &self.agent.mcp {
            self.agent
                .pending
                .extend(server.calls.try_iter().map(Pending::new));
        }
        if !self.agent.pending.is_empty() {
            let pending = std::mem::take(&mut self.agent.pending);
            self.agent.pending = self.answer_tools(pending);
            if !self.agent.pending.is_empty() {
                ctx.request_repaint_after(std::time::Duration::from_millis(200));
            }
        }
        loop {
            let Some(conn) = &self.agent.conn else { return };
            let Ok(message) = conn.agent.incoming.try_recv() else {
                return;
            };
            match message {
                Incoming::Response { id, result } => self.agent_response(id, result),
                Incoming::Notification { method, params } => {
                    if method == "session/update"
                        && let Some(update) = params.get("update")
                    {
                        self.agent.update(update);
                    }
                }
                Incoming::Request { id, method, params } => {
                    self.agent_request(id, &method, &params);
                }
                Incoming::Exited(tail) => {
                    self.agent.conn = None;
                    let mut text = "The agent stopped.".to_owned();
                    if !tail.trim().is_empty() {
                        text.push_str("\n\n");
                        text.push_str(tail.trim());
                    }
                    self.agent.notice(text, true);
                }
            }
        }
    }

    fn agent_response(&mut self, id: u64, result: Result<Value, RpcError>) {
        let Some(conn) = &mut self.agent.conn else {
            return;
        };
        match (&conn.phase, result) {
            (Phase::Starting(start), Ok(answer)) if *start == id => {
                let version = answer.get("protocolVersion").and_then(Value::as_u64);
                if version != Some(agent::PROTOCOL_VERSION) {
                    self.agent.conn = None;
                    self.agent.notice(
                        format!("The agent speaks ACP version {version:?}, not 1."),
                        true,
                    );
                    return;
                }
                // Violet's tools go over HTTP, which the agent says it takes.
                let http = answer
                    .pointer("/agentCapabilities/mcpCapabilities/http")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let servers = match (&self.agent.mcp, http) {
                    (Some(server), true) => json!([{
                        "type": "http",
                        "name": "violet",
                        "url": server.url,
                        "headers": [{"name": "Authorization", "value": format!("Bearer {}", server.token)}]
                    }]),
                    _ => json!([]),
                };
                let tools = servers
                    .as_array()
                    .is_some_and(|servers| !servers.is_empty());
                let cwd = self.agent_cwd();
                let Some(conn) = &mut self.agent.conn else {
                    return;
                };
                conn.tools = tools;
                let id = conn.agent.request(
                    "session/new",
                    json!({
                        "cwd": cwd.to_string_lossy(),
                        "mcpServers": servers,
                        "_meta": session_meta()
                    }),
                );
                conn.phase = Phase::Opening(id);
            }
            (Phase::Opening(open), Ok(answer)) if *open == id => {
                match answer.get("sessionId").and_then(Value::as_str) {
                    Some(session) => conn.phase = Phase::Ready(session.to_owned()),
                    None => {
                        self.agent.conn = None;
                        self.agent.notice("The agent opened no session.", true);
                    }
                }
            }
            (Phase::Starting(asked) | Phase::Opening(asked), Err(error)) if *asked == id => {
                self.agent.conn = None;
                let text = if error.code == agent::AUTH_REQUIRED {
                    "The agent needs you to log in. For Claude Code, run `claude` in a \
                     terminal and log in with /login, then restart the agent here."
                        .to_owned()
                } else {
                    format!("The agent could not start: {error}")
                };
                self.agent.notice(text, true);
            }
            (_, result) if conn.prompt == Some(id) => {
                conn.prompt = None;
                let stop = match result {
                    Ok(answer) => answer
                        .get("stopReason")
                        .and_then(Value::as_str)
                        .unwrap_or("end_turn")
                        .to_owned(),
                    Err(error) => {
                        self.agent.notice(error.to_string(), true);
                        return;
                    }
                };
                let note = match stop.as_str() {
                    "max_tokens" | "max_turn_requests" => Some("The agent stopped at its limit."),
                    "refusal" => Some("The agent declined to go on."),
                    "cancelled" => Some("Stopped."),
                    _ => None,
                };
                if let Some(note) = note {
                    self.agent.notice(note, false);
                }
            }
            _ => {}
        }
    }

    /// Answers what the agent asks of the client: no permission is
    /// given and no file is read or written.
    fn agent_request(&mut self, id: Value, method: &str, params: &Value) {
        let answer = match method {
            "session/request_permission" if asks_for_violet_tool(params) => {
                Ok(allowance(params).unwrap_or_else(|| refusal(params)))
            }
            "session/request_permission" => {
                let title = params
                    .pointer("/toolCall/title")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| {
                        let call = params.pointer("/toolCall/toolCallId")?.as_str()?;
                        self.agent.entries.iter().find_map(|entry| match entry {
                            Entry::Tool(tool) if tool.id == call => Some(tool.title.clone()),
                            _ => None,
                        })
                    })
                    .unwrap_or_else(|| "a tool".to_owned());
                self.agent.notice(
                    format!("Refused: {title}. Here the agent only writes queries."),
                    false,
                );
                Ok(refusal(params))
            }
            _ => Err(RpcError {
                code: -32601,
                message: format!("Violet does not answer {method}: the agent only writes queries"),
            }),
        };
        if let Some(conn) = &self.agent.conn {
            conn.agent.respond(&id, answer);
        }
    }

    /// Sends the typed message, with the active editor and its schema
    /// unless left out.
    pub(super) fn send_to_agent(&mut self) {
        let text = self.agent.input.trim().to_owned();
        let tools = self.agent.conn.as_ref().is_some_and(|conn| conn.tools);
        // With Violet's tools the agent reads the schema itself.
        let context = if self.agent.no_context {
            None
        } else if tools {
            self.editor_for_agent().ok()
        } else {
            self.agent_context()
        };
        let Some(conn) = &mut self.agent.conn else {
            return;
        };
        let Phase::Ready(session) = &conn.phase else {
            return;
        };
        if text.is_empty() || conn.prompt.is_some() {
            return;
        }
        let mut prompt = vec![json!({"type": "text", "text": text})];
        if let Some(context) = context {
            prompt.push(json!({"type": "text", "text": context}));
        }
        conn.prompt = Some(conn.agent.request(
            "session/prompt",
            json!({"sessionId": session, "prompt": prompt}),
        ));
        self.agent.entries.push(Entry::User(text));
        self.agent.input.clear();
    }

    /// The active editor, its connection and schema, as a note for the
    /// agent. Secrets in the editor are taken out; no rows are read.
    fn agent_context(&self) -> Option<String> {
        let tab = self.workspace.active().filter(|tab| tab.data().is_none())?;
        let language = tab.connection.map_or(Language::Sql, |id| self.language(id));
        let mut note = String::from(
            "Context from Violet, the database client the user is in. You cannot run \
             queries on their databases: write a statement in a ```sql block and the user \
             can insert it in the editor and run it there.\n",
        );
        if let Some(id) = tab.connection {
            let product = match self.explorer.link(id) {
                Link::Online(server) => {
                    format!("{} (database {})", server.version, server.database)
                }
                _ => "not connected".to_owned(),
            };
            note.push_str(&format!(
                "\nThe editor is on the connection \"{}\": {product}.\n",
                self.profile_name(id)
            ));
            let catalog = ConnectionCatalog {
                explorer: &self.explorer,
                id: Some(id),
            };
            let schemas = crate::ai::schemas_to_describe(
                self.explorer.search_path(id),
                crate::completion::Catalog::schemas(&catalog),
            );
            let schema = crate::ai::describe(&catalog, &schemas, language == Language::Sql);
            if !schema.text.is_empty() {
                note.push_str("\nSchema, as loaded in the tree:\n");
                note.push_str(&schema.text);
            }
        }
        let name = tab
            .file
            .as_ref()
            .map_or_else(|| tab.title.clone(), |file| file.path.display().to_string());
        let text = crate::lang::redact_secrets(language, &tab.sql);
        note.push_str(&format!(
            "\nThe editor \"{name}\" holds:\n```\n{text}\n```\n"
        ));
        if !tab.selection.is_empty()
            && let Some(selected) = tab.sql.get(tab.selection.clone())
        {
            let selected = crate::lang::redact_secrets(language, selected);
            note.push_str(&format!("\nSelected in it:\n```\n{selected}\n```\n"));
        }
        Some(note)
    }

    pub(super) fn cancel_agent(&mut self) {
        let Some(conn) = &mut self.agent.conn else {
            return;
        };
        let Phase::Ready(session) = &conn.phase else {
            return;
        };
        conn.agent
            .notify("session/cancel", json!({"sessionId": session}));
    }

    pub(super) fn set_agent_command(&mut self, ctx: &egui::Context, command: String) {
        self.settings.agent_command = command;
        self.save_settings(ctx);
        self.agent.setup = None;
        self.start_agent(ctx);
    }

    pub(super) fn clear_agent(&mut self, ctx: &egui::Context) {
        self.agent.entries.clear();
        self.start_agent(ctx);
    }
}

/// The answer that refuses a permission request: its reject option, or a
/// cancel when it offers none.
fn refusal(params: &Value) -> Value {
    let options = params.get("options").and_then(Value::as_array);
    let reject = ["reject_once", "reject_always"].iter().find_map(|kind| {
        options?
            .iter()
            .find(|option| option.get("kind").and_then(Value::as_str) == Some(kind))?
            .get("optionId")
            .cloned()
    });
    match reject {
        Some(option) => json!({"outcome": {"outcome": "selected", "optionId": option}}),
        None => json!({"outcome": {"outcome": "cancelled"}}),
    }
}

/// The panel's content.
pub fn show(ui: &mut Ui, panel: &mut AgentPanel, command: &str, actions: &mut Vec<Action>) {
    let palette = Palette::current(ui.ctx());
    let (state, busy) = status(panel.conn.as_ref());
    let working = panel
        .conn
        .as_ref()
        .is_some_and(|conn| conn.prompt.is_some());
    egui::Frame::new()
        .inner_margin(egui::Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Agent")
                        .font(theme::semibold(14.0))
                        .color(palette.text),
                );
                let color = if busy {
                    palette.warning
                } else if panel.conn.is_some() {
                    palette.success
                } else {
                    palette.dim
                };
                widgets::dot(ui, color, 3.5);
                ui.label(RichText::new(state).color(palette.dim));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if widgets::icon_button(ui, Icon::Close, "Close the panel").clicked() {
                        actions.push(Action::ToggleAgent);
                    }
                    if widgets::icon_button(ui, Icon::Settings, "Agent command").clicked() {
                        panel.setup = match panel.setup {
                            Some(_) => None,
                            None => Some(command.to_owned()),
                        };
                    }
                    if widgets::icon_button(ui, Icon::Plus, "New conversation").clicked() {
                        actions.push(Action::ClearAgent);
                    }
                    if panel.conn.is_some() {
                        if widgets::icon_button(ui, Icon::Power, "Stop the agent").clicked() {
                            actions.push(Action::StopAgent);
                        }
                    } else if widgets::icon_button(ui, Icon::Play, "Start the agent").clicked() {
                        actions.push(Action::StartAgent);
                    }
                });
            });
            if let Some(setup) = &mut panel.setup {
                ui.add_space(6.0);
                widgets::field(ui, "Command", |ui| {
                    ui.add(widgets::text_input(setup, agent::DEFAULT_COMMAND))
                });
                ui.label(
                    RichText::new(
                        "Any agent that speaks ACP. Claude Code's adapter needs Node, and \
                         Claude Code logged in.",
                    )
                    .color(palette.dim),
                );
                ui.horizontal(|ui| {
                    if widgets::button(ui, Tone::Secondary, Some(Icon::Save), "Save and restart")
                        .clicked()
                    {
                        actions.push(Action::SetAgentCommand(setup.trim().to_owned()));
                    }
                    if widgets::button(ui, Tone::Ghost, None, "Default").clicked() {
                        agent::DEFAULT_COMMAND.clone_into(setup);
                    }
                });
            }
        });
    ui.separator();

    let composer_height = 118.0;
    egui::ScrollArea::vertical()
        .id_salt("agent-log")
        .stick_to_bottom(true)
        .auto_shrink([false, false])
        .max_height((ui.available_height() - composer_height).max(80.0))
        .show(ui, |ui| {
            egui::Frame::new()
                .inner_margin(egui::Margin::symmetric(12, 8))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.spacing_mut().item_spacing.y = 8.0;
                    if panel.entries.is_empty() {
                        ui.label(
                            RichText::new(
                                "Ask about your data or have a query written. Messages \
                                 carry the active editor and its schema, never rows.",
                            )
                            .color(palette.dim),
                        );
                    }
                    for (index, entry) in panel.entries.iter().enumerate() {
                        entry_view(ui, &palette, index, entry, actions);
                    }
                    if working {
                        ui.horizontal(|ui| {
                            ui.add(egui::Spinner::new().size(12.0).color(palette.accent));
                            ui.label(RichText::new("Working…").color(palette.dim));
                        });
                    }
                });
        });

    ui.separator();
    egui::Frame::new()
        .inner_margin(egui::Margin::symmetric(12, 8))
        .show(ui, |ui| {
            let ready = panel
                .conn
                .as_ref()
                .is_some_and(|conn| matches!(conn.phase, Phase::Ready(_)));
            let id = egui::Id::new("agent-input");
            // Enter sends; Shift+Enter breaks the line.
            let send_key = ui.ctx().memory(|memory| memory.has_focus(id))
                && ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
            ui.add(
                egui::TextEdit::multiline(&mut panel.input)
                    .id(id)
                    .hint_text(if ready {
                        "Ask the agent…"
                    } else {
                        "The agent is not running"
                    })
                    .desired_rows(2)
                    .desired_width(f32::INFINITY)
                    .margin(egui::vec2(10.0, 8.0)),
            );
            ui.horizontal(|ui| {
                let mut context = !panel.no_context;
                if widgets::checkbox(ui, &mut context, "Editor and schema")
                    .on_hover_text(
                        "Send the active editor, its connection and the schema in the tree \
                         with each message. Never rows.",
                    )
                    .changed()
                {
                    panel.no_context = !context;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if working {
                        if widgets::button(ui, Tone::Secondary, Some(Icon::Stop), "Stop").clicked()
                        {
                            actions.push(Action::CancelAgent);
                        }
                    } else {
                        let can = ready && !panel.input.trim().is_empty();
                        let send = ui
                            .add_enabled_ui(can, |ui| {
                                widgets::button(ui, Tone::Primary, Some(Icon::Play), "Send")
                            })
                            .inner;
                        if send.clicked() || (send_key && can) {
                            actions.push(Action::SendToAgent);
                        }
                    }
                });
            });
        });
}

fn entry_view(
    ui: &mut Ui,
    palette: &Palette,
    index: usize,
    entry: &Entry,
    actions: &mut Vec<Action>,
) {
    match entry {
        Entry::User(text) => {
            egui::Frame::new()
                .fill(palette.accent_soft)
                .corner_radius(CornerRadius::same(8))
                .inner_margin(egui::Margin::symmetric(10, 7))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.add(
                        egui::Label::new(RichText::new(text).color(palette.text)).selectable(true),
                    );
                });
        }
        Entry::Agent { text, .. } => message(ui, palette, index, text, actions),
        Entry::Thought(text) => {
            egui::CollapsingHeader::new(RichText::new("Thinking").color(palette.dim))
                .id_salt(("agent-thought", index))
                .show(ui, |ui| {
                    ui.add(
                        egui::Label::new(RichText::new(text).color(palette.dim)).selectable(true),
                    );
                });
        }
        Entry::Tool(tool) => {
            let (icon, color) = match tool.status.as_str() {
                "completed" => (Icon::CircleCheck, palette.success),
                "failed" => (Icon::CircleAlert, palette.danger),
                _ => (Icon::Loader, palette.secondary),
            };
            let title = if tool.title.is_empty() {
                "Tool"
            } else {
                &tool.title
            };
            let header = egui::CollapsingHeader::new(RichText::new(title).color(palette.secondary))
                .id_salt(("agent-tool", index))
                .icon(move |ui, _, response| {
                    let rect = egui::Rect::from_center_size(
                        response.rect.center(),
                        egui::Vec2::splat(13.0),
                    );
                    widgets::paint_icon(ui, icon, rect, color);
                });
            header.show(ui, |ui| {
                for (path, lines) in &tool.diffs {
                    ui.label(
                        RichText::new(format!("{path} · {lines} lines"))
                            .font(theme::mono(11.5))
                            .color(palette.secondary),
                    );
                }
                if !tool.text.is_empty() {
                    let shown: String = tool.text.lines().take(30).collect::<Vec<_>>().join("\n");
                    code(ui, palette, &shown);
                }
            });
        }
        Entry::Plan(plan) => {
            egui::Frame::new()
                .stroke(Stroke::new(1.0, palette.outline))
                .corner_radius(CornerRadius::same(8))
                .inner_margin(egui::Margin::symmetric(10, 7))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    widgets::caption(ui, "Plan");
                    for (content, status) in plan {
                        let (mark, color) = match status.as_str() {
                            "completed" => ("✓", palette.success),
                            "in_progress" => ("›", palette.accent),
                            _ => ("·", palette.dim),
                        };
                        ui.horizontal_wrapped(|ui| {
                            ui.label(RichText::new(mark).color(color));
                            ui.label(RichText::new(content).color(palette.text));
                        });
                    }
                });
        }
        Entry::Notice { text, error } => {
            let color = if *error { palette.danger } else { palette.dim };
            ui.add(egui::Label::new(RichText::new(text).color(color)).selectable(true));
        }
    }
}

/// An agent's message: text, and its code blocks with a way to put SQL in
/// the editor.
fn message(ui: &mut Ui, palette: &Palette, index: usize, text: &str, actions: &mut Vec<Action>) {
    for (part, block) in split_fences(text).into_iter().enumerate() {
        match block {
            Block::Text(text) => {
                let text = text.trim();
                if !text.is_empty() {
                    ui.add(
                        egui::Label::new(RichText::new(text).color(palette.text)).selectable(true),
                    );
                }
            }
            Block::Code {
                language,
                code: body,
            } => {
                code(ui, palette, body);
                let sql = matches!(language, "" | "sql" | "SQL" | "psql" | "mysql" | "sqlite");
                if sql {
                    ui.push_id(("agent-code", index, part), |ui| {
                        ui.horizontal(|ui| {
                            if widgets::button(
                                ui,
                                Tone::Secondary,
                                Some(Icon::Terminal),
                                "Insert in editor",
                            )
                            .clicked()
                            {
                                actions.push(Action::InsertSql(body.to_owned()));
                            }
                            if widgets::icon_button(ui, Icon::Copy, "Copy").clicked() {
                                actions.push(Action::Copy(body.to_owned()));
                            }
                        });
                    });
                }
            }
        }
    }
}

fn code(ui: &mut Ui, palette: &Palette, text: &str) {
    egui::Frame::new()
        .fill(palette.panel)
        .stroke(Stroke::new(1.0, palette.outline))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(egui::Margin::same(8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.add(
                egui::Label::new(
                    RichText::new(text)
                        .font(theme::mono(12.0))
                        .color(palette.text),
                )
                .selectable(true),
            );
        });
}

#[derive(Debug, PartialEq, Eq)]
enum Block<'a> {
    Text(&'a str),
    Code { language: &'a str, code: &'a str },
}

/// `text` cut at its Markdown code fences. An unclosed fence, as while
/// the message streams in, runs to the end.
fn split_fences(text: &str) -> Vec<Block<'_>> {
    let mut blocks = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("```") {
        blocks.push(Block::Text(&rest[..start]));
        let after = &rest[start + 3..];
        let (language, body) = after.split_once('\n').unwrap_or((after, ""));
        match body.find("```") {
            Some(end) => {
                blocks.push(Block::Code {
                    language: language.trim(),
                    code: body[..end].trim_end(),
                });
                rest = &body[end + 3..];
            }
            None => {
                blocks.push(Block::Code {
                    language: language.trim(),
                    code: body.trim_end(),
                });
                rest = "";
            }
        }
    }
    blocks.push(Block::Text(rest));
    blocks
}

#[cfg(debug_assertions)]
impl VioletApp {
    /// Types and sends a message, for the `agent` scene.
    pub(super) fn agent_scene_send(&mut self, text: &str) -> bool {
        let ready = self
            .agent
            .conn
            .as_ref()
            .is_some_and(|conn| matches!(conn.phase, Phase::Ready(_)));
        if ready {
            text.clone_into(&mut self.agent.input);
            self.send_to_agent();
        }
        ready
    }

    /// Whether a turn was sent and has ended.
    pub(super) fn agent_scene_idle(&self) -> bool {
        self.agent
            .conn
            .as_ref()
            .is_some_and(|conn| conn.prompt.is_none())
            && self
                .agent
                .entries
                .iter()
                .any(|entry| matches!(entry, Entry::User(_)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permissions_are_refused() {
        let asked = json!({"options": [
            {"optionId": "yes", "kind": "allow_once"},
            {"optionId": "never", "kind": "reject_always"},
            {"optionId": "no", "kind": "reject_once"}
        ]});
        assert_eq!(refusal(&asked)["outcome"]["optionId"], "no");
        let only_allow = json!({"options": [{"optionId": "yes", "kind": "allow_once"}]});
        assert_eq!(refusal(&only_allow)["outcome"]["outcome"], "cancelled");
        // Violet's own tools are allowed; a name that only looks like one
        // is not.
        let ours = json!({
            "toolCall": {"title": "describe", "_meta": {"claudeCode": {"toolName": "mcp__violet__describe_schema"}}},
            "options": asked["options"]
        });
        assert!(asks_for_violet_tool(&ours));
        assert_eq!(
            allowance(&ours).expect("allowed")["outcome"]["optionId"],
            "yes"
        );
        let theirs = json!({"toolCall": {"title": "mcp__violet__run_query"}});
        assert!(!asks_for_violet_tool(&theirs));
        assert!(!asks_for_violet_tool(
            &json!({"toolCall": {"title": "Bash"}})
        ));
        let meta = session_meta();
        assert_eq!(meta["claudeCode"]["options"]["tools"], json!([]));
        assert_eq!(meta["claudeCode"]["options"]["settingSources"], json!([]));
        assert_eq!(
            meta["claudeCode"]["options"]["allowedTools"][0],
            "mcp__violet__list_connections"
        );
    }

    #[test]
    fn fences_split_out_code() {
        assert_eq!(
            split_fences("Try:\n```sql\nSELECT 1;\n```\nDone"),
            vec![
                Block::Text("Try:\n"),
                Block::Code {
                    language: "sql",
                    code: "SELECT 1;"
                },
                Block::Text("\nDone"),
            ]
        );
        assert_eq!(
            split_fences("```\nSELECT"),
            vec![
                Block::Text(""),
                Block::Code {
                    language: "",
                    code: "SELECT"
                },
                Block::Text(""),
            ]
        );
    }

    #[test]
    fn updates_build_the_conversation() {
        let mut panel = AgentPanel::default();
        for chunk in ["Hel", "lo"] {
            panel.update(&json!({
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": chunk}
            }));
        }
        panel.update(&json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "t1",
            "title": "Write totals.sql",
            "status": "pending"
        }));
        panel.update(&json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "t1",
            "status": "completed",
            "content": [{"type": "diff", "path": "/s/totals.sql", "oldText": null, "newText": "a\nb"}]
        }));
        assert_eq!(panel.entries.len(), 2);
        assert!(matches!(&panel.entries[0], Entry::Agent { text, .. } if text == "Hello"));
        let Entry::Tool(tool) = &panel.entries[1] else {
            panic!("a tool call");
        };
        assert_eq!(
            (
                tool.title.as_str(),
                tool.status.as_str(),
                tool.diffs.clone()
            ),
            (
                "Write totals.sql",
                "completed",
                vec![("/s/totals.sql".into(), 2)]
            )
        );
    }
}
