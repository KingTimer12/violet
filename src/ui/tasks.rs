//! Backup and restore, as DBeaver offers them: what to include, the
//! driver's own settings, where the file goes, the exact command before it
//! runs, and its log while it does.
//!
//! The settings come from the driver (a provider's `tools` export), so
//! every database brings its own; this view only knows how to draw a field.

use std::collections::{BTreeSet, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use egui::{CornerRadius, RichText, Sense, Stroke, Vec2};
use violet_app::tasks::{Program, TaskSummary};
use violet_app::{Offer, RequestId, TaskId};
use violet_core::{
    ConnectionId, ConnectionProfile, DriverId, Field, FieldKind, ObjectKind, ObjectRef, PathKind,
    Task, TaskKind, TaskRequest, TaskSettings, ToolCommand,
};

use super::action::Action;
use super::icons::Icon;
use super::state::{Explorer, Link, Loadable};
use super::task_flow::Tools;
use super::theme::{self, Palette};
use super::widgets::{self, Tone};
use super::workspace::format_duration;

/// Lines of log kept while a task runs.
const LOG_LINES: usize = 20_000;

/// The default backup file name.
pub const DEFAULT_NAME: &str = "${database}-${timestamp}";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Page {
    Objects,
    Settings,
    Files,
}

/// A backup or restore while its dialog is open.
pub struct TaskDialog {
    pub connection: ConnectionId,
    pub driver: DriverId,
    pub kind: TaskKind,
    page: Page,
    /// Field values; filled from the task's defaults (and what was used
    /// last) once it loads.
    pub settings: TaskSettings,
    initialized: bool,
    /// What to back up; empty for everything.
    pub objects: BTreeSet<ObjectRef>,
    open_schemas: HashSet<String>,
    filter: String,
    /// The folder a backup goes to, and its name with `${...}` variables.
    pub folder: String,
    pub name: String,
    /// When the dialog opened, for `${timestamp}`.
    stamp: jiff::Zoned,
    /// The file or folder to restore.
    pub input: String,
    inspected: Option<String>,
    /// The client programs' folder; empty to look in the usual places.
    pub client: String,
    programs_for: Option<String>,
    pub programs: Option<Vec<Program>>,
    /// Typed when the profile keeps no password.
    pub password: String,
    preview_key: String,
    pub preview_request: Option<RequestId>,
    pub preview: Option<Result<(PathBuf, ToolCommand), String>>,
    /// A restore asks for a second click.
    confirm: bool,
    pub run: Option<RunState>,
}

/// A task started from the dialog.
pub struct RunState {
    pub id: TaskId,
    pub started: Instant,
    pub path: PathBuf,
    pub log: VecDeque<String>,
    pub result: Option<Result<TaskSummary, String>>,
    /// The size of what a backup has written so far, checked now and then.
    size: Option<u64>,
    sized: Option<Instant>,
}

impl RunState {
    pub fn new(id: TaskId, path: PathBuf) -> Self {
        Self {
            id,
            started: Instant::now(),
            path,
            log: VecDeque::new(),
            result: None,
            size: None,
            sized: None,
        }
    }

    pub fn push(&mut self, lines: Vec<String>) {
        self.log.extend(lines);
        while self.log.len() > LOG_LINES {
            self.log.pop_front();
        }
    }
}

/// What the dialog reads from the app.
pub struct Env<'a> {
    pub profile: &'a ConnectionProfile,
    pub offer: Option<&'a Loadable<Offer>>,
    /// Its client programs being downloaded, or why they could not be.
    pub tools: Option<&'a Tools>,
    pub explorer: &'a Explorer,
    /// The user let this driver run programs.
    pub trusted: bool,
    pub driver_name: &'a str,
}

impl TaskDialog {
    pub fn new(
        connection: ConnectionId,
        driver: DriverId,
        kind: TaskKind,
        objects: Vec<ObjectRef>,
    ) -> Self {
        Self {
            connection,
            driver,
            kind,
            page: match kind {
                TaskKind::Backup => Page::Objects,
                TaskKind::Restore => Page::Files,
            },
            settings: TaskSettings::new(),
            initialized: false,
            open_schemas: objects
                .iter()
                .filter(|o| o.name.is_some())
                .map(|o| o.schema.clone())
                .collect(),
            objects: objects.into_iter().collect(),
            filter: String::new(),
            folder: String::new(),
            name: DEFAULT_NAME.to_owned(),
            stamp: jiff::Zoned::now(),
            input: String::new(),
            inspected: None,
            client: String::new(),
            programs_for: None,
            programs: None,
            password: String::new(),
            preview_key: String::new(),
            preview_request: None,
            preview: None,
            confirm: false,
            run: None,
        }
    }

    /// Fills the settings once the task is known: what was used last,
    /// within what the task accepts.
    pub fn initialize(&mut self, task: &Task, last: Option<&TaskSettings>) {
        if self.initialized {
            return;
        }
        self.settings = task.normalize(last.unwrap_or(&task.defaults()));
        self.initialized = true;
    }

    /// The file or folder the task writes or reads.
    pub fn path(&self, task: &Task, profile: &ConnectionProfile) -> Option<PathBuf> {
        match self.kind {
            TaskKind::Restore => {
                let input = self.input.trim();
                (!input.is_empty()).then(|| PathBuf::from(input))
            }
            TaskKind::Backup => {
                let folder = self.folder.trim();
                if folder.is_empty() {
                    return None;
                }
                let mut name = expand_name(&self.name, profile, &self.objects, &self.stamp);
                if name.is_empty() {
                    name = expand_name(DEFAULT_NAME, profile, &self.objects, &self.stamp);
                }
                let target = task.target(&self.settings);
                if target.kind == PathKind::File && !target.extension.is_empty() {
                    name = format!("{name}.{}", target.extension);
                }
                Some(Path::new(folder).join(name))
            }
        }
    }

    /// The request as it would run now.
    pub fn request(&self, task: &Task, profile: &ConnectionProfile) -> Option<TaskRequest> {
        let path = self.path(task, profile)?;
        Some(TaskRequest {
            kind: self.kind,
            settings: task.normalize(&self.settings),
            objects: if task.objects {
                self.objects.iter().cloned().collect()
            } else {
                Vec::new()
            },
            path: path.to_string_lossy().into_owned(),
        })
    }

    pub fn client_folder(&self) -> Option<PathBuf> {
        let client = self.client.trim();
        (!client.is_empty()).then(|| PathBuf::from(client))
    }

    /// A fresh timestamp and no result, to run the same task again.
    /// Looks for the programs and builds the command again.
    pub fn refresh(&mut self) {
        self.programs_for = None;
        self.preview_key.clear();
    }

    pub fn reset(&mut self) {
        self.run = None;
        self.stamp = jiff::Zoned::now();
        self.confirm = false;
        self.preview_key.clear();
    }

    /// Shows a page by name, for screenshot scenes.
    #[cfg(debug_assertions)]
    pub fn set_page(&mut self, page: &str) {
        self.page = match page {
            "objects" => Page::Objects,
            "settings" => Page::Settings,
            _ => Page::Files,
        };
    }

    pub fn running(&self) -> bool {
        self.run.as_ref().is_some_and(|run| run.result.is_none())
    }

    /// Takes the settings a restore file suggests, such as its format.
    pub fn apply_inspection(&mut self, path: &Path, settings: TaskSettings) {
        if self.input.trim() == path.to_string_lossy() {
            self.settings.extend(settings);
        }
    }
}

/// `${connection}`, `${database}`, `${host}`, `${schema}`, `${date}`,
/// `${time}` and `${timestamp}` filled in, and characters file systems
/// refuse replaced.
pub fn expand_name(
    template: &str,
    profile: &ConnectionProfile,
    objects: &BTreeSet<ObjectRef>,
    at: &jiff::Zoned,
) -> String {
    let schemas: BTreeSet<&str> = objects.iter().map(|o| o.schema.as_str()).collect();
    let schema = match schemas.len() {
        0 => "all".to_owned(),
        1 => schemas
            .iter()
            .next()
            .copied()
            .unwrap_or_default()
            .to_owned(),
        n => format!("{n}-schemas"),
    };
    let database = if profile.database.is_empty() {
        profile.username.as_str()
    } else {
        profile.database.as_str()
    };
    let name = template
        .replace("${connection}", &profile.name)
        .replace("${database}", database)
        .replace("${host}", &profile.host)
        .replace("${schema}", &schema)
        .replace("${date}", &at.strftime("%Y-%m-%d").to_string())
        .replace("${time}", &at.strftime("%H%M%S").to_string())
        .replace("${timestamp}", &at.strftime("%Y%m%d-%H%M%S").to_string());
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '_'
            } else {
                c
            }
        })
        .collect();
    cleaned.trim().trim_start_matches('.').to_owned()
}

/// Draws the dialog. Returns `false` once it should close.
pub fn show(
    ctx: &egui::Context,
    dialog: &mut TaskDialog,
    env: &Env<'_>,
    actions: &mut Vec<Action>,
) -> bool {
    let palette = Palette::current(ctx);
    let mut open = true;
    let modal = egui::Modal::new(egui::Id::new("violet-task"))
        .frame(
            egui::Frame::new()
                .fill(palette.surface)
                .stroke(Stroke::new(1.0, palette.outline))
                .corner_radius(CornerRadius::same(14))
                .shadow(ctx.global_style().visuals.window_shadow)
                .inner_margin(egui::Margin::same(22)),
        )
        .show(ctx, |ui| {
            ui.set_width(760.0);
            ui.set_max_width(760.0);
            ui.spacing_mut().item_spacing.y = 12.0;
            header(ui, dialog, env, &mut open, &palette);

            if !env.trusted {
                consent(ui, dialog, env, actions, &mut open, &palette);
                return;
            }
            let task = match env.offer {
                None | Some(Loadable::Idle | Loadable::Loading) => {
                    ui.allocate_ui(Vec2::new(ui.available_width(), 200.0), |ui| {
                        widgets::empty_state(
                            ui,
                            Icon::Loader,
                            "Loading",
                            &format!("Asking the {} provider what it offers…", env.driver_name),
                        );
                    });
                    return;
                }
                Some(Loadable::Failed(error)) => {
                    notice(ui, Icon::CircleAlert, palette.danger, error, &palette);
                    return;
                }
                Some(Loadable::Ready(offer)) => offer.tasks.iter().find(|t| t.kind == dialog.kind),
            };
            let Some(task) = task else {
                ui.allocate_ui(Vec2::new(ui.available_width(), 200.0), |ui| {
                    widgets::empty_state(
                        ui,
                        Icon::Info,
                        &format!("No {}", dialog.kind.label().to_lowercase()),
                        &format!(
                            "The {} provider does not offer it. Updating the provider may add it.",
                            env.driver_name
                        ),
                    );
                });
                return;
            };

            if let Some(run) = &mut dialog.run {
                progress(ui, dialog.kind, run, actions, &palette);
                return;
            }

            let pages: &[(Page, &str)] = match (dialog.kind, task.objects) {
                (TaskKind::Backup, true) => &[
                    (Page::Objects, "Objects"),
                    (Page::Settings, "Settings"),
                    (Page::Files, "Output"),
                ],
                (TaskKind::Backup, false) => {
                    &[(Page::Settings, "Settings"), (Page::Files, "Output")]
                }
                (TaskKind::Restore, _) => &[(Page::Files, "Input"), (Page::Settings, "Settings")],
            };
            if !pages.iter().any(|(page, _)| *page == dialog.page) {
                dialog.page = pages[0].0;
            }
            widgets::segmented(ui, &mut dialog.page, pages);

            egui::ScrollArea::vertical()
                .id_salt("task-body")
                .max_height(360.0)
                .min_scrolled_height(360.0)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 10.0;
                    match dialog.page {
                        Page::Objects => objects(ui, dialog, env, actions, &palette),
                        Page::Settings => settings(ui, dialog, task, &palette),
                        Page::Files => files(ui, dialog, task, env, actions, &palette),
                    }
                });

            request_updates(dialog, task, env, actions);
            footer(ui, dialog, task, env, actions, &palette);
        });
    // A stray click outside does not hide a running task; Close does.
    if modal.should_close() && !dialog.running() {
        open = false;
    }
    open
}

fn header(
    ui: &mut egui::Ui,
    dialog: &TaskDialog,
    env: &Env<'_>,
    open: &mut bool,
    palette: &Palette,
) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(36.0), Sense::hover());
        ui.painter()
            .rect_filled(rect, CornerRadius::same(10), palette.accent_soft);
        let icon = match dialog.kind {
            TaskKind::Backup => Icon::Archive,
            TaskKind::Restore => Icon::ArchiveRestore,
        };
        widgets::paint_icon(ui, icon, rect.shrink(9.0), palette.accent);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.label(
                RichText::new(format!("{} · {}", dialog.kind.label(), env.profile.name))
                    .font(theme::semibold(17.0))
                    .color(palette.text),
            );
            let database = if env.profile.database.is_empty() {
                String::new()
            } else {
                format!("{} on ", env.profile.database)
            };
            ui.label(
                RichText::new(format!(
                    "{} · {database}{}:{}",
                    env.driver_name, env.profile.host, env.profile.port
                ))
                .color(palette.secondary),
            );
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
            if widgets::icon_button(ui, Icon::Close, "Close").clicked() {
                *open = false;
            }
        });
    });
}

/// Asks once per driver before it may run programs outside its sandbox.
fn consent(
    ui: &mut egui::Ui,
    dialog: &TaskDialog,
    env: &Env<'_>,
    actions: &mut Vec<Action>,
    open: &mut bool,
    palette: &Palette,
) {
    let offer = match env.offer {
        Some(Loadable::Ready(offer)) => Some(offer),
        _ => None,
    };
    let programs = offer.map_or_else(
        || "its client programs".to_owned(),
        |offer| {
            let mut names: Vec<&str> = offer
                .tasks
                .iter()
                .flat_map(|task| task.programs.iter().map(String::as_str))
                .collect();
            names.dedup();
            names.join(", ")
        },
    );
    egui::Frame::new()
        .fill(palette.panel)
        .stroke(Stroke::new(1.0, palette.outline))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(egui::Margin::same(16))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::hover());
                widgets::paint_icon(ui, Icon::ShieldAlert, rect, palette.warning);
                ui.label(
                    RichText::new("Allow programs on this computer?")
                        .font(theme::semibold(14.0))
                        .color(palette.text),
                );
            });
            ui.label(
                RichText::new(format!(
                    "Backups and restores run {}'s own tools ({programs}) on this computer. \
                     Unlike the provider itself, which runs in a sandbox, they run as you: \
                     they read and write files and reach the server directly, with the \
                     connection's password.",
                    env.driver_name
                ))
                .color(palette.secondary),
            );
            ui.label(
                RichText::new(
                    "The provider cannot compose commands: it declares them up front, and \
                     Violet fills them in, passing the connection and password of this \
                     profile only. Shells and interpreters are never run, and each command \
                     shows in full before it starts.",
                )
                .color(palette.secondary),
            );
            if let Some(download) = offer.and_then(|offer| offer.download.as_ref()) {
                let host = download
                    .url
                    .trim_start_matches("https://")
                    .split('/')
                    .next()
                    .unwrap_or_default();
                ui.label(
                    RichText::new(format!(
                        "It downloads its own {} ({}) from {host}, checked against the \
                         digest the provider declares.",
                        download.label,
                        bytes(download.size)
                    ))
                    .color(palette.secondary),
                );
            }
            if let Some(offer) = offer {
                egui::CollapsingHeader::new(
                    RichText::new("Everything it may run").color(palette.secondary),
                )
                .id_salt("task-vocabulary")
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .max_height(160.0)
                        .show(ui, |ui| {
                            for task in &offer.tasks {
                                ui.label(
                                    RichText::new(format!(
                                        "{}: {}",
                                        task.kind.label(),
                                        task.programs.join(", ")
                                    ))
                                    .font(theme::medium(12.5))
                                    .color(palette.text),
                                );
                                ui.label(
                                    RichText::new(task.vocabulary().join("  "))
                                        .font(theme::mono(11.5))
                                        .color(palette.dim),
                                );
                            }
                        });
                });
            }
        });
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if widgets::button(ui, Tone::Primary, Some(Icon::Check), "Allow").clicked() {
                actions.push(Action::TrustTools(dialog.driver.clone()));
            }
            if widgets::button(ui, Tone::Ghost, None, "Cancel").clicked() {
                *open = false;
            }
        });
    });
}

fn notice(ui: &mut egui::Ui, icon: Icon, color: egui::Color32, text: &str, palette: &Palette) {
    egui::Frame::new()
        .fill(color.gamma_multiply(0.12))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(egui::Margin::symmetric(12, 9))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(15.0), Sense::hover());
                widgets::paint_icon(ui, icon, rect, color);
                ui.add(egui::Label::new(RichText::new(text).color(palette.text)).wrap());
            });
        });
}

/// Schemas and relations to include.
fn objects(
    ui: &mut egui::Ui,
    dialog: &mut TaskDialog,
    env: &Env<'_>,
    actions: &mut Vec<Action>,
    palette: &Palette,
) {
    let id = dialog.connection;
    let summary = summary(&dialog.objects);
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(summary)
                .font(theme::medium(13.0))
                .color(palette.text),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if !dialog.objects.is_empty()
                && widgets::button(ui, Tone::Ghost, None, "Whole database").clicked()
            {
                dialog.objects.clear();
            }
        });
    });

    let schemas = match (env.explorer.link(id), env.explorer.schemas.get(&id)) {
        (Link::Online(_), Some(Loadable::Ready(schemas))) => schemas.clone(),
        (Link::Online(_), Some(Loadable::Failed(error))) => {
            notice(ui, Icon::CircleAlert, palette.danger, error, palette);
            return;
        }
        (Link::Online(_), _) => {
            ui.label(RichText::new("Loading schemas…").color(palette.dim));
            return;
        }
        _ => {
            notice(
                ui,
                Icon::Info,
                palette.accent,
                "Connect to pick schemas and tables. Without a choice, the whole database is \
                 backed up.",
                palette,
            );
            if widgets::button(ui, Tone::Secondary, Some(Icon::Plug), "Connect").clicked() {
                actions.push(Action::Connect(id));
            }
            return;
        }
    };

    if dialog.objects.iter().any(|o| o.name.is_some()) {
        notice(
            ui,
            Icon::Info,
            palette.accent,
            "With single tables picked, the backup holds those tables only: not the schemas \
             around them, nor functions or types. Restore it where its schemas exist.",
            palette,
        );
    }
    ui.add(widgets::text_input(
        &mut dialog.filter,
        "Filter schemas and tables",
    ));
    let filter = dialog.filter.trim().to_lowercase();
    // Picks every schema the filter shows, whole; tables picked in them
    // give way to the schema.
    ui.horizontal(|ui| {
        let label = if filter.is_empty() {
            "Select all schemas"
        } else {
            "Select shown schemas"
        };
        if widgets::button(ui, Tone::Secondary, Some(Icon::Check), label).clicked() {
            for schema in schemas
                .iter()
                .filter(|schema| filter.is_empty() || schema.to_lowercase().contains(&filter))
            {
                dialog.objects.retain(|o| o.schema != *schema);
                dialog.objects.insert(ObjectRef {
                    schema: schema.clone(),
                    name: None,
                });
            }
        }
        if !dialog.objects.is_empty()
            && widgets::button(ui, Tone::Ghost, Some(Icon::Close), "Clear").clicked()
        {
            dialog.objects.clear();
        }
    });
    for schema in schemas.iter() {
        let key = (id, schema.clone());
        let loaded = match env.explorer.objects.get(&key) {
            Some(Loadable::Ready(objects)) => Some(objects.clone()),
            _ => None,
        };
        let relations: Vec<&violet_core::DbObject> = loaded
            .iter()
            .flat_map(|objects| objects.iter())
            .filter(|o| {
                matches!(
                    o.kind,
                    ObjectKind::Table
                        | ObjectKind::View
                        | ObjectKind::MaterializedView
                        | ObjectKind::ForeignTable
                        | ObjectKind::Sequence
                )
            })
            .filter(|o| {
                filter.is_empty()
                    || schema.to_lowercase().contains(&filter)
                    || o.name.to_lowercase().contains(&filter)
            })
            .collect();
        if !filter.is_empty() && !schema.to_lowercase().contains(&filter) && relations.is_empty() {
            continue;
        }

        let whole = ObjectRef {
            schema: schema.clone(),
            name: None,
        };
        let some = dialog
            .objects
            .iter()
            .any(|o| o.schema == *schema && o.name.is_some());
        let mut checked = dialog.objects.contains(&whole);
        let expanded = dialog.open_schemas.contains(schema);
        ui.horizontal(|ui| {
            let chevron = if expanded {
                Icon::ChevronDown
            } else {
                Icon::ChevronRight
            };
            if widgets::icon_button_sized(ui, chevron, "Show tables", 22.0, 14.0).clicked() {
                if expanded {
                    dialog.open_schemas.remove(schema);
                } else {
                    dialog.open_schemas.insert(schema.clone());
                    if matches!(
                        env.explorer.objects.get(&key),
                        None | Some(Loadable::Idle | Loadable::Failed(_))
                    ) {
                        actions.push(Action::LoadObjects(id, schema.clone()));
                    }
                }
            }
            let label = if some && !checked {
                format!("{schema}  (some tables)")
            } else {
                schema.clone()
            };
            let partly = some && !checked;
            if ui
                .add(widgets::Check::new(&mut checked, label).indeterminate(partly))
                .changed()
            {
                dialog.objects.retain(|o| o.schema != *schema);
                if checked {
                    dialog.objects.insert(whole.clone());
                }
            }
        });
        if !expanded {
            continue;
        }
        ui.indent(("task-schema", schema), |ui| match &loaded {
            None => {
                ui.label(RichText::new("Loading…").color(palette.dim));
            }
            Some(_) if relations.is_empty() => {
                ui.label(RichText::new("No tables").color(palette.dim));
            }
            Some(_) => {
                for relation in relations {
                    let item = ObjectRef {
                        schema: schema.clone(),
                        name: Some(relation.name.clone()),
                    };
                    let mut on = checked || dialog.objects.contains(&item);
                    ui.horizontal(|ui| {
                        let (rect, _) = ui.allocate_exact_size(Vec2::splat(14.0), Sense::hover());
                        widgets::paint_icon(ui, Icon::for_kind(relation.kind), rect, palette.dim);
                        let response =
                            ui.add_enabled(!checked, widgets::Check::new(&mut on, &relation.name));
                        if response.changed() {
                            if on {
                                dialog.objects.insert(item.clone());
                            } else {
                                dialog.objects.remove(&item);
                            }
                        }
                    });
                }
            }
        });
    }
}

fn summary(objects: &BTreeSet<ObjectRef>) -> String {
    if objects.is_empty() {
        return "Whole database".to_owned();
    }
    let schemas = objects.iter().filter(|o| o.name.is_none()).count();
    let tables = objects.len() - schemas;
    let plural =
        |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    match (schemas, tables) {
        (0, t) => plural(t, "table", "tables"),
        (s, 0) => plural(s, "schema", "schemas"),
        (s, t) => format!(
            "{} and {}",
            plural(s, "schema", "schemas"),
            plural(t, "table", "tables")
        ),
    }
}

/// The driver's fields, by section.
fn settings(ui: &mut egui::Ui, dialog: &mut TaskDialog, task: &Task, palette: &Palette) {
    let mut sections: Vec<&str> = Vec::new();
    for field in &task.fields {
        if !sections.contains(&field.section.as_str()) {
            sections.push(&field.section);
        }
    }
    for section in sections {
        let fields: Vec<&Field> = task
            .fields
            .iter()
            .filter(|f| f.section == section && f.visible(&dialog.settings))
            .collect();
        if fields.is_empty() {
            continue;
        }
        widgets::caption(ui, section);
        egui::Grid::new(("task-section", section))
            .num_columns(2)
            .spacing([18.0, 8.0])
            .min_col_width(200.0)
            .show(ui, |ui| {
                for field in fields {
                    let value = dialog
                        .settings
                        .entry(field.id.clone())
                        .or_insert_with(|| field.default.clone());
                    field_row(ui, field, value, palette);
                    ui.end_row();
                }
            });
        ui.add_space(4.0);
    }
}

fn field_row(ui: &mut egui::Ui, field: &Field, value: &mut String, palette: &Palette) {
    let label = |ui: &mut egui::Ui| {
        let response = ui.label(RichText::new(&field.label).color(palette.secondary));
        if !field.help.is_empty() {
            response.on_hover_text(&field.help);
        }
    };
    match &field.kind {
        FieldKind::Flag => {
            ui.label("");
            let mut on = value == "true";
            let response = widgets::checkbox(ui, &mut on, &field.label);
            if response.changed() {
                *value = on.to_string();
            }
            if !field.help.is_empty() {
                response.on_hover_text(&field.help);
            }
        }
        FieldKind::Text => {
            label(ui);
            ui.add(widgets::text_input(value, "").desired_width(360.0));
        }
        FieldKind::Number { min, max } => {
            label(ui);
            let mut number: i64 = value.parse().unwrap_or(*min);
            if ui
                .add(egui::DragValue::new(&mut number).range(*min..=*max))
                .changed()
            {
                *value = number.to_string();
            }
        }
        FieldKind::Choice(choices) => {
            label(ui);
            let current = choices
                .iter()
                .find(|c| c.value == *value)
                .map_or(value.as_str(), |c| c.label.as_str())
                .to_owned();
            egui::ComboBox::from_id_salt(("task-field", &field.id))
                .selected_text(current)
                .width(360.0)
                .show_ui(ui, |ui| {
                    for choice in choices {
                        ui.selectable_value(value, choice.value.clone(), &choice.label);
                    }
                });
        }
    }
}

/// Where the backup goes, or what to restore, and the client programs.
fn files(
    ui: &mut egui::Ui,
    dialog: &mut TaskDialog,
    task: &Task,
    env: &Env<'_>,
    actions: &mut Vec<Action>,
    palette: &Palette,
) {
    let target = task.target(&dialog.settings);
    match dialog.kind {
        TaskKind::Backup => {
            widgets::caption(ui, "Output");
            ui.horizontal(|ui| {
                ui.add(
                    widgets::text_input(&mut dialog.folder, "Folder to write the backup to")
                        .desired_width(560.0),
                );
                if widgets::button(ui, Tone::Secondary, Some(Icon::FolderOpen), "Choose…").clicked()
                {
                    let mut picker = rfd::FileDialog::new().set_title("Back up into");
                    if !dialog.folder.trim().is_empty() {
                        picker = picker.set_directory(dialog.folder.trim());
                    }
                    if let Some(folder) = picker.pick_folder() {
                        dialog.folder = folder.to_string_lossy().into_owned();
                    }
                }
            });
            widgets::field(ui, "File name", |ui| {
                ui.add(widgets::text_input(&mut dialog.name, DEFAULT_NAME))
            })
            .on_hover_text(
                "Variables: ${connection}, ${database}, ${host}, ${schema}, ${date}, \
                 ${time}, ${timestamp}",
            );
            let path = dialog.path(task, env.profile);
            let what = if target.kind == PathKind::Directory {
                "Writes the folder"
            } else {
                "Writes"
            };
            match path {
                Some(path) => {
                    ui.add(
                        egui::Label::new(
                            RichText::new(format!("{what} {}", path.display()))
                                .font(theme::mono(12.0))
                                .color(palette.dim),
                        )
                        .wrap(),
                    );
                    if path.exists() {
                        notice(
                            ui,
                            Icon::CircleAlert,
                            palette.warning,
                            "It already exists and will be replaced.",
                            palette,
                        );
                    }
                }
                None => {
                    ui.label(RichText::new("Choose a folder.").color(palette.dim));
                }
            }
        }
        TaskKind::Restore => {
            widgets::caption(ui, "Input");
            ui.horizontal(|ui| {
                ui.add(
                    widgets::text_input(&mut dialog.input, "The backup to restore")
                        .desired_width(470.0),
                );
                if widgets::button(ui, Tone::Secondary, Some(Icon::FileCode), "File…").clicked()
                    && let Some(file) = rfd::FileDialog::new().set_title("Restore from").pick_file()
                {
                    dialog.input = file.to_string_lossy().into_owned();
                }
                if widgets::button(ui, Tone::Secondary, Some(Icon::FolderOpen), "Folder…")
                    .on_hover_text("For backups in the directory format")
                    .clicked()
                    && let Some(folder) = rfd::FileDialog::new()
                        .set_title("Restore from folder")
                        .pick_folder()
                {
                    dialog.input = folder.to_string_lossy().into_owned();
                }
            });
            let input = dialog.input.trim().to_owned();
            if !input.is_empty() {
                let path = Path::new(&input);
                if !path.exists() {
                    ui.label(RichText::new("Not found.").color(palette.danger));
                } else if dialog.inspected.as_deref() != Some(input.as_str()) {
                    dialog.inspected = Some(input.clone());
                    actions.push(Action::TaskInspect(path.to_path_buf()));
                } else if let Some(format) = format_label(task, &dialog.settings) {
                    ui.label(RichText::new(format!("Format: {format}")).color(palette.secondary));
                }
            }
            if task.fields.iter().any(|f| f.id == "format")
                && dialog.settings.get("format").map(String::as_str) == Some("plain")
            {
                notice(
                    ui,
                    Icon::ShieldAlert,
                    palette.warning,
                    "Plain SQL runs in the database's console program, which can also run \
                     commands written in the file. Only restore files you trust.",
                    palette,
                );
            }
            let database = dialog
                .settings
                .get("database")
                .map(|d| d.trim())
                .filter(|d| !d.is_empty())
                .unwrap_or(&env.profile.database);
            notice(
                ui,
                Icon::Info,
                palette.accent,
                &format!(
                    "Restores into {} on {}. Settings → Input changes the database.",
                    if database.is_empty() {
                        "the default database"
                    } else {
                        database
                    },
                    env.profile.host
                ),
                palette,
            );
        }
    }

    if !env.profile.save_password {
        widgets::caption(ui, "Connection");
        widgets::field(ui, "Password", |ui| {
            ui.add(
                widgets::text_input(&mut dialog.password, "The connection's password")
                    .password(true)
                    .desired_width(360.0),
            )
        });
    }

    widgets::caption(ui, "Client programs");
    ui.horizontal(|ui| {
        ui.add(
            widgets::text_input(&mut dialog.client, "Downloaded or found automatically")
                .desired_width(560.0),
        );
        if widgets::button(ui, Tone::Secondary, Some(Icon::FolderOpen), "Choose…").clicked()
            && let Some(folder) = rfd::FileDialog::new()
                .set_title("Folder with the client programs")
                .pick_folder()
        {
            dialog.client = folder.to_string_lossy().into_owned();
        }
    });
    let server = match env.explorer.link(dialog.connection) {
        Link::Online(server) => Some(server.version.as_str()),
        _ => None,
    };
    match env.tools {
        Some(Tools::Downloading { label, size }) => {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(14.0).color(palette.accent));
                ui.label(
                    RichText::new(format!("Downloading {label} ({})…", bytes(*size)))
                        .color(palette.secondary),
                );
            });
            ui.ctx().request_repaint_after(Duration::from_millis(250));
        }
        Some(Tools::Failed(error)) => {
            notice(
                ui,
                Icon::CircleAlert,
                palette.warning,
                &format!("The client programs could not be downloaded: {error}"),
                palette,
            );
            if widgets::button(ui, Tone::Secondary, Some(Icon::Refresh), "Try again").clicked() {
                actions.push(Action::RetryTools(dialog.driver.clone()));
            }
        }
        None => {}
    }
    match &dialog.programs {
        None => {
            ui.label(RichText::new("Looking for the programs…").color(palette.dim));
        }
        Some(programs) => {
            for program in programs {
                program_row(ui, program, server, palette);
            }
        }
    }
}

/// The label of the format a restore file was found to be in.
fn format_label(task: &Task, settings: &TaskSettings) -> Option<String> {
    let field = task.fields.iter().find(|f| f.id == "format")?;
    let value = settings.get("format")?;
    match &field.kind {
        FieldKind::Choice(choices) => choices
            .iter()
            .find(|c| c.value == *value)
            .map(|c| c.label.clone()),
        _ => Some(value.clone()),
    }
}

fn program_row(ui: &mut egui::Ui, program: &Program, server: Option<&str>, palette: &Palette) {
    ui.horizontal(|ui| {
        let (icon, color) = if program.path.is_some() {
            (Icon::CircleCheck, palette.success)
        } else {
            (Icon::CircleAlert, palette.danger)
        };
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(14.0), Sense::hover());
        widgets::paint_icon(ui, icon, rect, color);
        ui.label(
            RichText::new(&program.name)
                .font(theme::mono(12.5))
                .color(palette.text),
        );
        match &program.path {
            Some(path) => {
                if let Some(version) = &program.version {
                    ui.label(RichText::new(version).color(palette.secondary));
                }
                ui.add(
                    egui::Label::new(
                        RichText::new(path.display().to_string())
                            .font(theme::mono(12.0))
                            .color(palette.dim),
                    )
                    .truncate(),
                )
                .on_hover_text(path.display().to_string());
            }
            None => {
                ui.label(RichText::new("not found").color(palette.danger));
            }
        }
    });
    if let (Some(client), Some(server)) = (
        program.version.as_deref().and_then(major),
        server.and_then(major),
    ) && client < server
    {
        notice(
            ui,
            Icon::CircleAlert,
            palette.warning,
            &format!(
                "{} is version {client}, older than the server ({server}); it may refuse to \
                 work. Pick a newer client folder.",
                program.name
            ),
            palette,
        );
    }
}

/// The first whole number in a version string.
fn major(version: &str) -> Option<u32> {
    let start = version.find(|c: char| c.is_ascii_digit())?;
    let digits: String = version[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// Asks for the command preview and the program lookup when what they
/// depend on changed.
fn request_updates(dialog: &mut TaskDialog, task: &Task, env: &Env<'_>, actions: &mut Vec<Action>) {
    let client = dialog.client.trim().to_owned();
    if dialog.programs_for.as_deref() != Some(client.as_str()) {
        dialog.programs_for = Some(client);
        dialog.programs = None;
        actions.push(Action::TaskFindPrograms);
    }
    let key = format!(
        "{:?}",
        (
            &dialog.settings,
            &dialog.objects,
            dialog.path(task, env.profile),
            &dialog.client
        )
    );
    if key != dialog.preview_key {
        dialog.preview_key = key;
        dialog.confirm = false;
        if dialog.path(task, env.profile).is_some() {
            actions.push(Action::TaskPreview);
        } else {
            dialog.preview = None;
            dialog.preview_request = None;
        }
    }
}

fn footer(
    ui: &mut egui::Ui,
    dialog: &mut TaskDialog,
    task: &Task,
    env: &Env<'_>,
    actions: &mut Vec<Action>,
    palette: &Palette,
) {
    let ready = match &dialog.preview {
        Some(Ok((program, command))) => {
            let text = violet_app::tasks::display(program, command);
            egui::Frame::new()
                .fill(palette.panel)
                .stroke(Stroke::new(1.0, palette.outline))
                .corner_radius(CornerRadius::same(8))
                .inner_margin(egui::Margin::symmetric(12, 9))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        widgets::caption(ui, "Command");
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if widgets::icon_button_sized(ui, Icon::Copy, "Copy", 22.0, 13.0)
                                .clicked()
                            {
                                actions.push(Action::Copy(text.clone()));
                            }
                        });
                    });
                    egui::ScrollArea::vertical()
                        .id_salt("task-command")
                        .max_height(72.0)
                        .show(ui, |ui| {
                            ui.add(
                                egui::Label::new(
                                    RichText::new(&text)
                                        .font(theme::mono(11.5))
                                        .color(palette.secondary),
                                )
                                .wrap()
                                .selectable(true),
                            );
                        });
                });
            true
        }
        Some(Err(error)) => {
            notice(ui, Icon::CircleAlert, palette.danger, error, palette);
            false
        }
        None => {
            let text = if dialog.path(task, env.profile).is_none() {
                match dialog.kind {
                    TaskKind::Backup => "Choose where the backup goes in Output.",
                    TaskKind::Restore => "Choose the backup to restore in Input.",
                }
            } else {
                "Preparing the command…"
            };
            ui.label(RichText::new(text).color(palette.dim));
            false
        }
    };

    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let start = match dialog.kind {
                TaskKind::Backup => ui.add_enabled_ui(ready, |ui| {
                    widgets::button(ui, Tone::Primary, Some(Icon::Archive), "Back up")
                }),
                TaskKind::Restore => {
                    let label = if dialog.confirm {
                        "Click again to restore"
                    } else {
                        "Restore"
                    };
                    ui.add_enabled_ui(ready, |ui| {
                        widgets::button(ui, Tone::Danger, Some(Icon::ArchiveRestore), label)
                    })
                }
            }
            .inner;
            if start.clicked() {
                if dialog.kind == TaskKind::Restore && !dialog.confirm {
                    dialog.confirm = true;
                } else {
                    dialog.confirm = false;
                    actions.push(Action::TaskStart);
                }
            }
            if widgets::button(ui, Tone::Ghost, None, "Reset settings")
                .on_hover_text("Back to the provider's defaults")
                .clicked()
            {
                dialog.settings = task.defaults();
            }
            if dialog.kind == TaskKind::Restore && dialog.confirm {
                ui.label(
                    RichText::new("Objects in the database may be replaced.")
                        .color(palette.warning),
                );
            }
        });
    });
}

/// The log of a running or finished task.
fn progress(
    ui: &mut egui::Ui,
    kind: TaskKind,
    run: &mut RunState,
    actions: &mut Vec<Action>,
    palette: &Palette,
) {
    let elapsed = match &run.result {
        Some(Ok(summary)) => summary.elapsed,
        _ => run.started.elapsed(),
    };
    // Backups grow on disk: show how far along they are.
    if kind == TaskKind::Backup
        && run.result.is_none()
        && run
            .sized
            .is_none_or(|at| at.elapsed() > Duration::from_millis(700))
    {
        run.size = size(&run.path);
        run.sized = Some(Instant::now());
    }

    match &run.result {
        None => {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(16.0).color(palette.accent));
                let doing = match kind {
                    TaskKind::Backup => "Backing up",
                    TaskKind::Restore => "Restoring",
                };
                let mut text = format!("{doing}… {}", format_duration(elapsed));
                if let Some(size) = run.size {
                    text.push_str(&format!(" · {}", bytes(size)));
                }
                ui.label(
                    RichText::new(text)
                        .font(theme::medium(13.0))
                        .color(palette.text),
                );
            });
            ui.ctx().request_repaint_after(Duration::from_millis(250));
        }
        Some(Ok(summary)) => {
            let mut text = format!(
                "{} finished in {}",
                kind.label(),
                format_duration(summary.elapsed)
            );
            if let Some(size) = summary.bytes.filter(|_| kind == TaskKind::Backup) {
                text.push_str(&format!(" · {}", bytes(size)));
            }
            notice(ui, Icon::CircleCheck, palette.success, &text, palette);
        }
        Some(Err(error)) if error == "Cancelled" => {
            let text = match kind {
                TaskKind::Backup => "Stopped. What was written so far is left in place.",
                TaskKind::Restore => {
                    "Stopped. What was restored so far stays, unless it ran in a single \
                     transaction."
                }
            };
            notice(ui, Icon::Info, palette.warning, text, palette);
        }
        Some(Err(error)) => {
            notice(ui, Icon::CircleAlert, palette.danger, error, palette);
        }
    }

    egui::Frame::new()
        .fill(palette.panel)
        .stroke(Stroke::new(1.0, palette.outline))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(egui::Margin::symmetric(12, 9))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::ScrollArea::vertical()
                .id_salt("task-log")
                .max_height(330.0)
                .min_scrolled_height(330.0)
                .auto_shrink([false, false])
                .stick_to_bottom(true)
                .show_rows(ui, 16.0, run.log.len().max(1), |ui, rows| {
                    if run.log.is_empty() {
                        ui.label(
                            RichText::new("Waiting for output…")
                                .font(theme::mono(11.5))
                                .color(palette.dim),
                        );
                        return;
                    }
                    for line in run.log.range(rows) {
                        let lower = line.to_lowercase();
                        let color = if lower.contains("error") || lower.contains("fatal") {
                            palette.danger
                        } else if lower.contains("warning") {
                            palette.warning
                        } else {
                            palette.secondary
                        };
                        ui.label(RichText::new(line).font(theme::mono(11.5)).color(color));
                    }
                });
        });

    ui.horizontal(|ui| {
        ui.with_layout(
            egui::Layout::right_to_left(egui::Align::Center),
            |ui| match &run.result {
                None => {
                    if widgets::button(ui, Tone::Danger, Some(Icon::Stop), "Stop").clicked() {
                        actions.push(Action::TaskCancel);
                    }
                }
                Some(result) => {
                    if widgets::button(ui, Tone::Primary, None, "Done").clicked() {
                        actions.push(Action::TaskClose);
                    }
                    if widgets::button(ui, Tone::Secondary, Some(Icon::Refresh), "Run again")
                        .clicked()
                    {
                        actions.push(Action::TaskAgain);
                    }
                    if result.is_ok()
                        && kind == TaskKind::Backup
                        && widgets::button(
                            ui,
                            Tone::Secondary,
                            Some(Icon::FolderOpen),
                            "Show in folder",
                        )
                        .clicked()
                    {
                        actions.push(Action::Reveal(run.path.clone()));
                    }
                    if widgets::icon_button(ui, Icon::Copy, "Copy the log").clicked() {
                        let log: Vec<&str> = run.log.iter().map(String::as_str).collect();
                        actions.push(Action::Copy(log.join("\n")));
                    }
                }
            },
        );
    });
}

/// A file's size, or the total of a folder's files.
fn size(path: &Path) -> Option<u64> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.is_file() {
        return Some(meta.len());
    }
    Some(
        std::fs::read_dir(path)
            .ok()?
            .flatten()
            .filter_map(|entry| entry.metadata().ok())
            .filter(std::fs::Metadata::is_file)
            .map(|meta| meta.len())
            .sum(),
    )
}

pub fn bytes(size: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = size as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{size} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Opens the system file manager at `path`.
pub fn reveal(path: &Path) {
    let result = if cfg!(target_os = "macos") {
        std::process::Command::new("open")
            .arg("-R")
            .arg(path)
            .spawn()
    } else if cfg!(windows) {
        let mut select = std::ffi::OsString::from("/select,");
        select.push(path);
        std::process::Command::new("explorer").arg(select).spawn()
    } else {
        let folder = path.parent().unwrap_or(path);
        std::process::Command::new("xdg-open").arg(folder).spawn()
    };
    if let Err(error) = result {
        log::warn!("could not show {}: {error}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use violet_core::DriverId;

    #[test]
    fn names_fill_variables_and_stay_one_file() {
        let mut profile = ConnectionProfile::new(DriverId::new("postgres"), 5432);
        profile.name = "Shop: prod/eu".into();
        profile.database = "shop".into();
        let at: jiff::Zoned = "2026-10-14T15:04:05[America/Sao_Paulo]"
            .parse()
            .expect("time");
        let mut objects = BTreeSet::new();
        assert_eq!(
            expand_name(DEFAULT_NAME, &profile, &objects, &at),
            "shop-20261014-150405"
        );
        objects.insert(ObjectRef {
            schema: "sales".into(),
            name: Some("orders".into()),
        });
        assert_eq!(
            expand_name("${connection} ${schema} ${date}", &profile, &objects, &at),
            "Shop_ prod_eu sales 2026-10-14"
        );
        assert_eq!(expand_name("../../etc", &profile, &objects, &at), "_.._etc");
        assert_eq!(summary(&objects), "1 table");
        assert_eq!(bytes(1536), "1.5 KB");
        assert_eq!(major("pg_dump (PostgreSQL) 16.4"), Some(16));
        assert_eq!(major("PostgreSQL 17.11 on aarch64"), Some(17));
    }
}
