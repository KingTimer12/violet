//! Debug builds only: drive the interface through a scene and save a
//! screenshot, for checking the look without clicking by hand.
//!
//! ```sh
//! VIOLET_HOME=/tmp/violet VIOLET_SNAPSHOT=/tmp/shot.png VIOLET_SCENE=browse cargo run
//! ```
//!
//! Scenes: `welcome` (default), `browse` (connect the first profile, open
//! a table and its data), `error` (run a broken statement), `form` (the
//! connection dialog), `data` (a table's data tab, filtered and sorted),
//! `menu` (the data tab with a connection's context menu open), `page`
//! (a data tab after fetching the next page and counting), `providers`
//! (the providers dialog), `complete` (completion at the `|` of
//! `VIOLET_SCENE_SQL`), `explain` (`EXPLAIN ANALYZE` of a join),
//! `history` (a run, then the history dialog), `restore` (the editors
//! saved last time), `task` (the backup or restore dialog: see
//! [`VioletApp::task_scene`]), `edit` (a data tab of `VIOLET_SCENE_TABLE`,
//! double-clicked at `VIOLET_SCENE_X`, `VIOLET_SCENE_Y`), `sort` (runs
//! `VIOLET_SCENE_SQL`, clicks a header at `VIOLET_SCENE_X`, `_Y` and a menu
//! item at `VIOLET_SCENE_APPLY_X`, `_Y`). A profile without
//! a saved password connects with `VIOLET_SCENE_PASSWORD`. Add
//! `VIOLET_THEME=light` for the light theme.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use violet_core::ObjectKind;

use super::VioletApp;
use super::action::{Action, RunScope};
use super::state::{Link, NodeKey};
use super::workspace::{Outcome, TabKind};

pub struct Snapshot {
    path: PathBuf,
    scene: String,
    started: Instant,
    step: usize,
    requested: bool,
    /// Input to feed the next frame, as if the user did it.
    events: Vec<egui::Event>,
    /// Where the `complete` scene puts the cursor, in characters.
    cursor: usize,
}

impl Snapshot {
    pub fn from_env() -> Option<Self> {
        let path = std::env::var_os("VIOLET_SNAPSHOT")?.into();
        Some(Self {
            path,
            scene: std::env::var("VIOLET_SCENE").unwrap_or_else(|_| "welcome".to_owned()),
            started: Instant::now(),
            step: 0,
            requested: false,
            events: Vec::new(),
            cursor: 0,
        })
    }

    /// Whether the scene starts from the editors saved last time; the
    /// others start from none.
    pub fn restores(&self) -> bool {
        self.scene == "restore"
    }

    /// Hands queued input to the frame about to start.
    pub fn inject(&mut self, raw: &mut egui::RawInput) {
        raw.events.append(&mut self.events);
    }
}

/// A right click at `pos`, pressed or released.
fn secondary(pos: egui::Pos2, pressed: bool) -> egui::Event {
    egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Secondary,
        pressed,
        modifiers: egui::Modifiers::NONE,
    }
}

/// A key press.
fn key(key: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
    egui::Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers,
    }
}

impl VioletApp {
    pub(super) fn drive_snapshot(&mut self, ctx: &egui::Context) {
        let Some(mut snapshot) = self.snapshot.take() else {
            return;
        };
        ctx.request_repaint();
        let elapsed = snapshot.started.elapsed();

        // Save the screenshot once it arrives, then quit.
        let image = ctx.input(|input| {
            input.events.iter().find_map(|event| match event {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(image) = image {
            let [width, height] = image.size;
            let pixels: Vec<u8> = image
                .pixels
                .iter()
                .flat_map(|pixel| pixel.to_array())
                .collect();
            let saved = image::RgbaImage::from_raw(width as u32, height as u32, pixels)
                .map(|buffer| buffer.save(&snapshot.path));
            log::info!("snapshot saved: {saved:?}");
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }

        let ready = self.advance_scene(ctx, &mut snapshot, elapsed);
        if (ready || elapsed > Duration::from_secs(20)) && !snapshot.requested {
            snapshot.requested = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
        }
        self.snapshot = Some(snapshot);
    }

    /// Takes the scene one step further. Returns `true` when it is ready to
    /// be captured.
    fn advance_scene(
        &mut self,
        ctx: &egui::Context,
        snapshot: &mut Snapshot,
        elapsed: Duration,
    ) -> bool {
        let first = self.profiles.all().first().map(|profile| profile.id);
        let settle = elapsed > Duration::from_millis(1_500);
        match (snapshot.scene.as_str(), snapshot.step) {
            ("form", 0) => {
                self.actions.push(Action::NewConnection);
                snapshot.step += 1;
                false
            }
            ("providers", 0) => {
                self.actions.push(Action::OpenProviders);
                snapshot.step += 1;
                false
            }
            ("providers", _) => {
                settle && matches!(self.providers.listing, super::providers::Listing::Ready(_))
            }
            (
                "browse" | "error" | "data" | "menu" | "page" | "complete" | "explain" | "history"
                | "task" | "edit" | "sort",
                0,
            ) => {
                if let Some(id) = first {
                    self.explorer.expanded.insert(NodeKey::Connection(id));
                    self.explorer
                        .expanded
                        .insert(NodeKey::Folder(id, super::state::Folder::Schemas));
                    // A profile whose password is not saved connects with
                    // `VIOLET_SCENE_PASSWORD`.
                    match std::env::var("VIOLET_SCENE_PASSWORD") {
                        Ok(password) => self.actions.push(Action::ConnectWithPassword(
                            id,
                            violet_core::Secret::new(password),
                        )),
                        Err(_) => self.actions.push(Action::Connect(id)),
                    }
                }
                snapshot.step += 1;
                false
            }
            ("browse", 1) => {
                let Some(id) = first else { return true };
                let loaded = self
                    .explorer
                    .objects
                    .contains_key(&(id, "public".to_owned()));
                if loaded && self.explorer.link(id).is_online() {
                    self.explorer.expanded.insert(NodeKey::Group(
                        id,
                        "public".into(),
                        ObjectKind::Table,
                    ));
                    self.explorer.expanded.insert(NodeKey::Group(
                        id,
                        "public".into(),
                        ObjectKind::View,
                    ));
                    let customers = NodeKey::Object(id, "public".into(), "customers".into());
                    self.explorer.expanded.insert(customers.clone());
                    self.explorer.selected = Some(customers);
                    self.actions
                        .push(Action::LoadColumns(id, "public".into(), "customers".into()));
                    self.actions.push(Action::OpenEditor {
                        connection: Some(id),
                        sql: "-- Top customers by revenue\nSELECT c.name,\n       c.country,\n       count(o.id)   AS orders,\n       sum(o.total)  AS revenue\nFROM customers c\nJOIN orders o ON o.customer_id = c.id\nWHERE o.status IS NOT NULL\nGROUP BY c.id\nORDER BY revenue DESC\nLIMIT 50;\n".into(),
                    });
                    snapshot.step += 1;
                }
                false
            }
            ("browse", 2) => {
                if let Some(tab) = self.workspace.active {
                    self.actions.push(Action::Run(tab, RunScope::Script));
                }
                snapshot.step += 1;
                false
            }
            ("data" | "menu" | "page", 1) => {
                let Some(id) = first else { return true };
                let loaded = self
                    .explorer
                    .objects
                    .contains_key(&(id, "public".to_owned()));
                if loaded && self.explorer.link(id).is_online() {
                    self.explorer.expanded.insert(NodeKey::Group(
                        id,
                        "public".into(),
                        ObjectKind::Table,
                    ));
                    self.actions.push(Action::OpenData {
                        connection: id,
                        schema: "public".into(),
                        relation: "customers".into(),
                    });
                    snapshot.step += 1;
                }
                false
            }
            ("sort", 1) => {
                let Some(id) = first else { return true };
                if self.explorer.link(id).is_online() {
                    let sql = std::env::var("VIOLET_SCENE_SQL")
                        .unwrap_or_else(|_| "SELECT * FROM customers".into());
                    self.actions.push(Action::OpenEditor {
                        connection: Some(id),
                        sql,
                    });
                    snapshot.step += 1;
                }
                false
            }
            ("sort", 2) => {
                if let Some(tab) = self.workspace.active {
                    self.actions.push(Action::Run(tab, RunScope::Script));
                    snapshot.step += 1;
                }
                false
            }
            // Click the header at `VIOLET_SCENE_X`, `_Y`, then the menu item
            // at `VIOLET_SCENE_APPLY_X`, `_Y`, one event a frame.
            ("sort", step @ 3..=8) => {
                let done = self.workspace.active().is_some_and(|tab| {
                    tab.running.is_none() && matches!(tab.outcome, Some(Outcome::Done { .. }))
                });
                if !done {
                    return false;
                }
                if step >= 6 && std::env::var("VIOLET_SCENE_APPLY_X").is_err() {
                    return elapsed > Duration::from_secs(8);
                }
                let at = |name: &str| {
                    std::env::var(name)
                        .ok()
                        .and_then(|value| value.parse::<f32>().ok())
                        .unwrap_or_default()
                };
                let pos = if step < 6 {
                    egui::pos2(at("VIOLET_SCENE_X"), at("VIOLET_SCENE_Y"))
                } else {
                    egui::pos2(at("VIOLET_SCENE_APPLY_X"), at("VIOLET_SCENE_APPLY_Y"))
                };
                snapshot.events.push(match step % 3 {
                    0 => egui::Event::PointerMoved(pos),
                    phase => egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed: phase == 1,
                        modifiers: egui::Modifiers::NONE,
                    },
                });
                snapshot.step += 1;
                false
            }
            ("sort", _) => elapsed > Duration::from_secs(10),
            ("edit", 1) => {
                let Some(id) = first else { return true };
                let loaded = self
                    .explorer
                    .objects
                    .contains_key(&(id, "public".to_owned()));
                if loaded && self.explorer.link(id).is_online() {
                    let table =
                        std::env::var("VIOLET_SCENE_TABLE").unwrap_or_else(|_| "customers".into());
                    self.actions.push(Action::OpenData {
                        connection: id,
                        schema: "public".into(),
                        relation: table,
                    });
                    snapshot.step += 1;
                }
                false
            }
            ("edit", 2) => {
                // Once the rows can be edited, double-click the first cell
                // of the second column.
                let Some(tab) = self.workspace.active else {
                    return false;
                };
                let editing = self.editing(tab);
                log::info!(
                    "edit scene: {}",
                    match &editing {
                        super::edit_flow::Editing::On(_) => "on".to_owned(),
                        super::edit_flow::Editing::Loading => "loading".to_owned(),
                        super::edit_flow::Editing::Off(reason) => format!("off: {reason}"),
                    }
                );
                if matches!(editing, super::edit_flow::Editing::On(_)) {
                    let x: f32 = std::env::var("VIOLET_SCENE_X")
                        .ok()
                        .and_then(|x| x.parse().ok())
                        .unwrap_or(420.0);
                    let y: f32 = std::env::var("VIOLET_SCENE_Y")
                        .ok()
                        .and_then(|y| y.parse().ok())
                        .unwrap_or(200.0);
                    snapshot
                        .events
                        .push(egui::Event::PointerMoved(egui::pos2(x, y)));
                    snapshot.step += 1;
                }
                false
            }
            // Press, release, press, release: one a frame, as a hand would.
            ("edit", step @ 3..=6) => {
                let x: f32 = std::env::var("VIOLET_SCENE_X")
                    .ok()
                    .and_then(|x| x.parse().ok())
                    .unwrap_or(420.0);
                let y: f32 = std::env::var("VIOLET_SCENE_Y")
                    .ok()
                    .and_then(|y| y.parse().ok())
                    .unwrap_or(200.0);
                snapshot.events.push(egui::Event::PointerButton {
                    pos: egui::pos2(x, y),
                    button: egui::PointerButton::Primary,
                    pressed: step % 2 == 1,
                    modifiers: egui::Modifiers::NONE,
                });
                snapshot.step += 1;
                false
            }
            // With `VIOLET_SCENE_VALUE`: put it in the cell, review the
            // change, and click at `VIOLET_SCENE_APPLY_X`, `_Y` (the
            // review's apply button) when given.
            ("edit", 7) => {
                let Ok(value) = std::env::var("VIOLET_SCENE_VALUE") else {
                    return settle && elapsed > Duration::from_secs(6);
                };
                let Some(editor) = self.cell_editor.take() else {
                    return false;
                };
                self.actions.push(Action::SetCell(
                    editor.tab,
                    editor.row,
                    editor.column,
                    Some(value),
                ));
                self.actions.push(Action::ReviewEdits(editor.tab));
                snapshot.step += 1;
                false
            }
            ("edit", 8) => {
                let at = |name: &str| {
                    std::env::var(name)
                        .ok()
                        .and_then(|value| value.parse::<f32>().ok())
                };
                let (Some(x), Some(y)) = (at("VIOLET_SCENE_APPLY_X"), at("VIOLET_SCENE_APPLY_Y"))
                else {
                    return elapsed > Duration::from_secs(8);
                };
                if self.edit_review.is_none() {
                    return false;
                }
                let pos = egui::pos2(x, y);
                snapshot.events.push(egui::Event::PointerMoved(pos));
                snapshot.step += 1;
                false
            }
            ("edit", step @ 9..=10) => {
                let at = |name: &str| {
                    std::env::var(name)
                        .ok()
                        .and_then(|value| value.parse::<f32>().ok())
                        .unwrap_or_default()
                };
                snapshot.events.push(egui::Event::PointerButton {
                    pos: egui::pos2(at("VIOLET_SCENE_APPLY_X"), at("VIOLET_SCENE_APPLY_Y")),
                    button: egui::PointerButton::Primary,
                    pressed: step == 9,
                    modifiers: egui::Modifiers::NONE,
                });
                snapshot.step += 1;
                false
            }
            ("edit", _) => elapsed > Duration::from_secs(12),
            ("data" | "menu", 2) => {
                // Filter and sort once the first page is in.
                let Some(tab) = self.workspace.active else {
                    return false;
                };
                let loaded = self.workspace.get(tab).is_some_and(|tab| {
                    tab.running.is_none() && matches!(tab.outcome, Some(Outcome::Done { .. }))
                });
                if loaded {
                    if let Some(tab) = self.workspace.get_mut(tab)
                        && let TabKind::Data(view) = &mut tab.kind
                    {
                        view.filter = "country IN ('BR', 'PT')".into();
                        view.cycle_order("credit");
                        view.cycle_order("credit");
                    }
                    self.actions.push(Action::ApplyFilter(tab));
                    snapshot.step += 1;
                }
                false
            }
            ("page", 2) => {
                let Some(tab) = self.workspace.active else {
                    return false;
                };
                let loaded = self.workspace.get(tab).is_some_and(|tab| {
                    tab.running.is_none() && matches!(tab.outcome, Some(Outcome::Done { .. }))
                });
                if loaded {
                    self.actions.push(Action::FetchMore(tab));
                    self.actions.push(Action::CountRows(tab));
                    snapshot.step = 10;
                    snapshot.started = Instant::now();
                }
                false
            }
            ("page", _) => {
                let ready = self.workspace.active().is_some_and(|tab| {
                    tab.running.is_none()
                        && tab.paging.as_ref().is_some_and(|p| {
                            matches!(p.total, Some(super::workspace::Count::Done(_)))
                        })
                });
                ready && elapsed > Duration::from_millis(5_000)
            }
            ("data", 3) => {
                // Back to the unfiltered first page: nothing changed on the
                // server, so it comes from the cache.
                let Some(tab) = self.workspace.active else {
                    return false;
                };
                let done = self.workspace.get(tab).is_some_and(|tab| {
                    tab.running.is_none() && matches!(tab.outcome, Some(Outcome::Done { .. }))
                });
                if done {
                    if let Some(tab) = self.workspace.get_mut(tab)
                        && let TabKind::Data(view) = &mut tab.kind
                    {
                        view.order = None;
                    }
                    self.actions.push(Action::ClearFilter(tab));
                    snapshot.step += 1;
                }
                false
            }
            ("menu", 3) => {
                let done = self.workspace.active().is_some_and(|tab| {
                    tab.running.is_none() && matches!(tab.outcome, Some(Outcome::Done { .. }))
                });
                if done {
                    // Right-click the connection, the first row of the tree.
                    let pos = egui::pos2(120.0, 152.0);
                    snapshot.events.push(egui::Event::PointerMoved(pos));
                    snapshot.events.push(secondary(pos, true));
                    snapshot.step += 1;
                }
                false
            }
            ("menu", 4) => {
                snapshot
                    .events
                    .push(secondary(egui::pos2(120.0, 152.0), false));
                snapshot.step = 10;
                snapshot.started = Instant::now() - Duration::from_millis(1_000);
                false
            }
            ("complete", 1) => {
                let Some(id) = first else { return true };
                let loaded = self
                    .explorer
                    .objects
                    .contains_key(&(id, "public".to_owned()));
                if loaded && self.explorer.link(id).is_online() {
                    let sql = std::env::var("VIOLET_SCENE_SQL")
                        .unwrap_or_else(|_| "SELECT *\nFROM customers c\nJOIN |".to_owned());
                    let cursor = sql.find('|').unwrap_or(sql.len());
                    self.actions.push(Action::OpenEditor {
                        connection: Some(id),
                        sql: sql.replacen('|', "", 1),
                    });
                    snapshot.step = 2;
                    snapshot.cursor = sql[..cursor].chars().count();
                }
                false
            }
            ("complete", 2) => {
                // Put the cursor in place and press ⌃Space.
                let Some(tab) = self.workspace.active else {
                    return false;
                };
                let text_id = egui::Id::new(("violet-editor", tab));
                let mut state =
                    egui::text_edit::TextEditState::load(ctx, text_id).unwrap_or_default();
                state
                    .cursor
                    .set_char_range(Some(egui::text::CCursorRange::one(
                        egui::text::CCursor::new(snapshot.cursor),
                    )));
                state.store(ctx, text_id);
                ctx.memory_mut(|memory| memory.request_focus(text_id));
                // `VIOLET_SCENE_TYPE` types text instead, as a user would.
                match std::env::var("VIOLET_SCENE_TYPE") {
                    Ok(text) => snapshot.events.push(egui::Event::Text(text)),
                    Err(_) => snapshot
                        .events
                        .push(key(egui::Key::Space, egui::Modifiers::CTRL)),
                }
                snapshot.step = 3;
                snapshot.started = Instant::now();
                false
            }
            ("complete", 3) => {
                // `VIOLET_SCENE_ACCEPT` presses Enter once the list is up.
                if elapsed > Duration::from_millis(1_500) {
                    if std::env::var_os("VIOLET_SCENE_ACCEPT").is_some() {
                        snapshot
                            .events
                            .push(key(egui::Key::Enter, egui::Modifiers::NONE));
                    }
                    snapshot.step = 10;
                    snapshot.started = Instant::now() - Duration::from_millis(1_000);
                }
                false
            }
            ("complete", _) => settle,
            ("explain" | "history", 1) => {
                let Some(id) = first else { return true };
                if self.explorer.link(id).is_online() {
                    let sql = if snapshot.scene == "explain" {
                        "SELECT c.country, count(*) AS orders\nFROM customers c\nJOIN orders o ON o.customer_id = c.id\nGROUP BY c.country;"
                    } else {
                        "SELECT count(*) FROM orders WHERE status = 'paid';"
                    };
                    self.actions.push(Action::OpenEditor {
                        connection: Some(id),
                        sql: sql.to_owned(),
                    });
                    snapshot.step = 2;
                }
                false
            }
            ("explain" | "history", 2) => {
                if let Some(tab) = self.workspace.active {
                    let scope = if snapshot.scene == "explain" {
                        RunScope::ExplainAnalyze
                    } else {
                        RunScope::Statement
                    };
                    self.actions.push(Action::Run(tab, scope));
                }
                snapshot.step = 3;
                false
            }
            ("explain" | "history", 3) => {
                let done = self
                    .workspace
                    .active()
                    .is_some_and(|tab| tab.running.is_none() && tab.outcome.is_some());
                if done {
                    if snapshot.scene == "history" {
                        self.actions.push(Action::OpenHistory);
                    }
                    snapshot.step = 10;
                    snapshot.started = Instant::now() - Duration::from_millis(800);
                }
                false
            }
            ("explain" | "history", _) => settle,
            ("error", 1) => {
                let Some(id) = first else { return true };
                if matches!(self.explorer.link(id), Link::Online(_)) {
                    self.actions.push(Action::OpenEditor {
                        connection: Some(id),
                        sql: "SELECT id, name, emial\nFROM customers\nWHERE country = 'BR';".into(),
                    });
                    snapshot.step += 1;
                }
                false
            }
            ("error", 2) => {
                if let Some(tab) = self.workspace.active {
                    self.actions.push(Action::Run(tab, RunScope::Script));
                }
                snapshot.step += 1;
                false
            }
            ("browse" | "error" | "data", _) => {
                let done = self.workspace.active().is_some_and(|tab| {
                    tab.running.is_none()
                        && matches!(
                            tab.outcome,
                            Some(Outcome::Done { .. } | Outcome::Failed { .. })
                        )
                });
                if done && snapshot.step < 10 {
                    snapshot.step = 10;
                    snapshot.started = Instant::now() - Duration::from_millis(1_000);
                }
                snapshot.step >= 10 && settle
            }
            ("menu", _) => snapshot.step >= 10 && settle,
            ("task", _) => self.task_scene(snapshot, first),
            _ => settle,
        }
    }

    /// The `task` scene, from environment variables:
    ///
    /// - `VIOLET_SCENE_TASK`: `backup` (default) or `restore`;
    /// - `VIOLET_SCENE_PAGE`: `objects`, `settings` or `files`;
    /// - `VIOLET_SCENE_OBJECTS`: `schema` or `schema.table`, comma separated;
    /// - `VIOLET_SCENE_FOLDER`, `VIOLET_SCENE_NAME`, `VIOLET_SCENE_INPUT`,
    ///   `VIOLET_SCENE_CLIENT`: the output folder and name, the file to
    ///   restore and the client programs' folder;
    /// - `VIOLET_SCENE_SET`: settings as `id=value`, comma separated;
    /// - `VIOLET_SCENE_CONSENT`: show the first-use question instead of
    ///   allowing the provider;
    /// - `VIOLET_SCENE_RUN`: start the task and capture its end;
    ///   `VIOLET_SCENE_STOP` stops it after that many milliseconds.
    fn task_scene(
        &mut self,
        snapshot: &mut Snapshot,
        first: Option<violet_core::ConnectionId>,
    ) -> bool {
        use violet_core::{ObjectRef, TaskKind};
        let env = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        let Some(id) = first else { return true };
        match snapshot.step {
            1 => {
                let online = self.explorer.link(id).is_online()
                    && matches!(
                        self.explorer.schemas.get(&id),
                        Some(super::state::Loadable::Ready(_))
                    );
                if !online {
                    return false;
                }
                let kind = if env("VIOLET_SCENE_TASK").as_deref() == Some("restore") {
                    TaskKind::Restore
                } else {
                    TaskKind::Backup
                };
                let objects: Vec<ObjectRef> = env("VIOLET_SCENE_OBJECTS")
                    .map(|text| {
                        text.split(',')
                            .map(|item| match item.split_once('.') {
                                Some((schema, name)) => ObjectRef {
                                    schema: schema.to_owned(),
                                    name: Some(name.to_owned()),
                                },
                                None => ObjectRef {
                                    schema: item.to_owned(),
                                    name: None,
                                },
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                if let Some(profile) = self.profiles.get(id)
                    && env("VIOLET_SCENE_CONSENT").is_none()
                {
                    self.settings
                        .trusted_tools
                        .insert(profile.driver.to_string());
                }
                for object in &objects {
                    self.actions
                        .push(Action::LoadObjects(id, object.schema.clone()));
                }
                self.open_task(id, kind, objects);
                if let Some(dialog) = &mut self.task_dialog {
                    if let Some(folder) = env("VIOLET_SCENE_FOLDER") {
                        dialog.folder = folder;
                    }
                    if let Some(name) = env("VIOLET_SCENE_NAME") {
                        dialog.name = name;
                    }
                    if let Some(input) = env("VIOLET_SCENE_INPUT") {
                        dialog.input = input;
                    }
                    if let Some(client) = env("VIOLET_SCENE_CLIENT") {
                        dialog.client = client;
                    }
                    if let Some(page) = env("VIOLET_SCENE_PAGE") {
                        dialog.set_page(&page);
                    }
                }
                snapshot.step += 1;
                false
            }
            2 => {
                // Settings apply once the provider described its tasks.
                let Some(dialog) = &mut self.task_dialog else {
                    return true;
                };
                if dialog.settings.is_empty() && env("VIOLET_SCENE_CONSENT").is_none() {
                    return false;
                }
                for pair in env("VIOLET_SCENE_SET").unwrap_or_default().split(',') {
                    if let Some((key, value)) = pair.split_once('=') {
                        dialog
                            .settings
                            .insert(key.trim().into(), value.trim().into());
                    }
                }
                snapshot.step += 1;
                snapshot.started = Instant::now();
                false
            }
            3 => {
                let Some(dialog) = &self.task_dialog else {
                    return true;
                };
                let ready = matches!(dialog.preview, Some(Ok(_))) && dialog.programs.is_some();
                let waited = snapshot.started.elapsed() > Duration::from_millis(2_500);
                if env("VIOLET_SCENE_RUN").is_some() {
                    if ready && waited {
                        // The action itself: the dialog's second click for a
                        // restore is the button's business.
                        self.actions.push(Action::TaskStart);
                        snapshot.step += 1;
                        snapshot.started = Instant::now();
                    }
                    return false;
                }
                waited
            }
            _ => {
                let Some(run) = self.task_dialog.as_ref().and_then(|d| d.run.as_ref()) else {
                    return snapshot.started.elapsed() > Duration::from_secs(2);
                };
                if let Some(stop) = env("VIOLET_SCENE_STOP").and_then(|ms| ms.parse().ok())
                    && run.result.is_none()
                    && snapshot.started.elapsed() > Duration::from_millis(stop)
                    && snapshot.step == 4
                {
                    self.actions.push(Action::TaskCancel);
                    snapshot.step += 1;
                }
                run.result.is_some() && run.started.elapsed() > Duration::from_millis(300)
            }
        }
    }
}
