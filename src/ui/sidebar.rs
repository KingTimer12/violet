//! The connections tree: connections, schemas, object groups, objects and
//! columns, loaded as they are expanded.

use std::path::PathBuf;

use egui::{Align2, Color32, CornerRadius, CursorIcon, DragAndDrop, Response, Sense, Ui, Vec2};
use violet_core::{
    ConnectionId, ConnectionProfile, DbObject, Language, ObjectKind, ObjectRef, TaskKind,
};
use violet_storage::{SavedScript, ScriptFolder};

use super::action::Action;
use super::catalog_flow;
use super::definition_flow::Family;
use super::icons::Icon;
use super::key_flow::KeyOp;
use super::state::{Explorer, Folder, FolderItem, Link, Loadable, NodeKey, TreeFilter};
use super::theme::{self, Palette};
use super::widgets;

const ROW_HEIGHT: f32 = 26.0;
const INDENT: f32 = 14.0;

/// One row of the tree.
struct Row<'a> {
    depth: usize,
    /// `Some(expanded)` when the row has children.
    expanded: Option<bool>,
    icon: Icon,
    icon_tint: Option<Color32>,
    label: &'a str,
    detail: Option<&'a str>,
    selected: bool,
    loading: bool,
    /// A dot before the icon, for connection status.
    status: Option<Color32>,
    strong: bool,
    /// Whether the row can be dragged onto another.
    draggable: bool,
}

/// What is being dragged across the tree.
#[derive(Clone)]
enum DragItem {
    Connection {
        id: ConnectionId,
        folder: String,
        name: String,
    },
    Folder(String),
    ScriptFolder {
        connection: ConnectionId,
        database: String,
        folder: String,
    },
    Script {
        path: PathBuf,
        connection: ConnectionId,
        database: String,
        folder: String,
        name: String,
    },
}

impl DragItem {
    fn name(&self) -> &str {
        match self {
            Self::Connection { name, .. } | Self::Script { name, .. } => name,
            Self::Folder(path) | Self::ScriptFolder { folder: path, .. } => {
                path.rsplit('/').next().unwrap_or(path)
            }
        }
    }
}

/// Highlights the row while something `accept` has an action for hovers
/// it, and pushes that action when it is dropped there.
fn drop_target(
    ui: &Ui,
    response: &Response,
    actions: &mut Vec<Action>,
    accept: impl Fn(&DragItem) -> Option<Action>,
) {
    let Some(item) = response.dnd_hover_payload::<DragItem>() else {
        return;
    };
    if accept(&item).is_none() {
        return;
    }
    let palette = Palette::current(ui.ctx());
    ui.painter().rect_stroke(
        response.rect.shrink2(Vec2::new(6.0, 1.0)),
        CornerRadius::same(6),
        egui::Stroke::new(1.5, palette.accent),
        egui::StrokeKind::Inside,
    );
    if let Some(item) = response.dnd_release_payload::<DragItem>()
        && let Some(action) = accept(&item)
    {
        actions.push(action);
    }
}

/// Moves to the top level what may go there.
fn to_root(item: &DragItem) -> Option<Action> {
    match item {
        DragItem::Connection { id, folder, .. } if !folder.is_empty() => {
            Some(Action::MoveToFolder(*id, String::new()))
        }
        DragItem::Folder(path) => violet_app::moved_folder(path, "")
            .map(|_| Action::MoveFolder(path.clone(), String::new())),
        _ => None,
    }
}

/// Moves into connection folder `target` what may go there.
fn to_folder(item: &DragItem, target: &str) -> Option<Action> {
    match item {
        DragItem::Connection { id, folder, .. } if folder != target => {
            Some(Action::MoveToFolder(*id, target.to_owned()))
        }
        DragItem::Folder(path) => violet_app::moved_folder(path, target)
            .map(|_| Action::MoveFolder(path.clone(), target.to_owned())),
        _ => None,
    }
}

/// Moves into script folder `target` of `database` (any, at the top level)
/// what may go there: a script never leaves its database.
fn to_script_folder(
    item: &DragItem,
    connection: ConnectionId,
    database: Option<&str>,
    target: &str,
) -> Option<Action> {
    match item {
        DragItem::Script {
            path,
            connection: from,
            database: db,
            folder,
            ..
        } if *from == connection
            && database.is_none_or(|database| database == db)
            && folder != target =>
        {
            Some(Action::MoveScript(path.clone(), target.to_owned()))
        }
        DragItem::ScriptFolder {
            connection: from,
            database: db,
            folder,
        } if *from == connection
            && database.is_none_or(|database| database == db)
            && violet_app::moved_folder(folder, target).is_some() =>
        {
            Some(Action::MoveScriptFolder(
                connection,
                db.clone(),
                folder.clone(),
                target.to_owned(),
            ))
        }
        _ => None,
    }
}

/// Follows the pointer with the dragged item's name.
fn drag_preview(ui: &Ui) {
    let Some(item) = DragAndDrop::payload::<DragItem>(ui.ctx()) else {
        return;
    };
    let Some(pointer) = ui.ctx().pointer_latest_pos() else {
        return;
    };
    ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
    let palette = Palette::current(ui.ctx());
    let painter = ui.ctx().layer_painter(egui::LayerId::new(
        egui::Order::Tooltip,
        egui::Id::new("sidebar-drag"),
    ));
    let galley = painter.layout_no_wrap(
        item.name().to_owned(),
        egui::FontId::proportional(13.0),
        palette.text,
    );
    let rect = egui::Rect::from_min_size(pointer + Vec2::new(14.0, 6.0), galley.size())
        .expand2(Vec2::new(8.0, 4.0));
    painter.rect_filled(rect, CornerRadius::same(6), palette.surface);
    painter.rect_stroke(
        rect,
        CornerRadius::same(6),
        egui::Stroke::new(1.0, palette.accent),
        egui::StrokeKind::Inside,
    );
    painter.galley(rect.min + Vec2::new(8.0, 4.0), galley, palette.text);
}

impl<'a> Row<'a> {
    fn new(depth: usize, icon: Icon, label: &'a str) -> Self {
        Self {
            depth,
            expanded: None,
            icon,
            icon_tint: None,
            label,
            detail: None,
            selected: false,
            loading: false,
            status: None,
            strong: false,
            draggable: false,
        }
    }

    fn show(self, ui: &mut Ui) -> Response {
        self.show_with_toggle(ui).0
    }

    /// Draws the row; the flag is set when the click landed on the chevron,
    /// so a row can expand from there and do something else when clicked
    /// anywhere else.
    fn show_with_toggle(self, ui: &mut Ui) -> (Response, bool) {
        let palette = Palette::current(ui.ctx());
        let sense = if self.draggable {
            Sense::click_and_drag()
        } else {
            Sense::click()
        };
        let (rect, response) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_HEIGHT), sense);
        let chevron_end = rect.left() + 6.0 + 6.0 + self.depth as f32 * INDENT + 18.0;
        let toggled = self.expanded.is_some()
            && response.clicked()
            && response
                .interact_pointer_pos()
                .is_some_and(|pos| pos.x <= chevron_end);
        if !ui.is_rect_visible(rect) {
            return (response, toggled);
        }
        let painter = ui.painter();
        let row = rect.shrink2(Vec2::new(6.0, 1.0));
        if self.selected {
            painter.rect_filled(row, CornerRadius::same(6), palette.accent_soft);
        } else if response.hovered() {
            painter.rect_filled(row, CornerRadius::same(6), palette.surface_hover);
        }

        let mut x = row.left() + 6.0 + self.depth as f32 * INDENT;
        let center = row.center().y;
        if let Some(expanded) = self.expanded {
            let chevron = if expanded {
                Icon::ChevronDown
            } else {
                Icon::ChevronRight
            };
            let chevron_rect =
                egui::Rect::from_center_size(egui::pos2(x + 6.0, center), Vec2::splat(13.0));
            widgets::paint_icon(ui, chevron, chevron_rect, palette.dim);
        }
        x += 16.0;
        if let Some(status) = self.status {
            painter.circle_filled(egui::pos2(x + 3.0, center), 3.5, status);
            x += 11.0;
        }
        let icon_rect =
            egui::Rect::from_center_size(egui::pos2(x + 8.0, center), Vec2::splat(15.0));
        if self.loading {
            let angle = ui.input(|input| input.time) as f32 * 6.0;
            let image = Icon::Loader
                .image(palette.accent, 15.0)
                .rotate(angle, Vec2::splat(0.5));
            image.paint_at(ui, icon_rect);
            ui.ctx().request_repaint();
        } else {
            let tint = self.icon_tint.unwrap_or(if self.selected {
                palette.accent
            } else {
                palette.secondary
            });
            widgets::paint_icon(ui, self.icon, icon_rect, tint);
        }
        x += 24.0;

        let detail_width = self.detail.map_or(0.0, |detail| {
            let galley = painter.layout_no_wrap(detail.to_owned(), theme::mono(11.0), palette.dim);
            let width = galley.size().x.min(row.width() * 0.4);
            let detail_rect = egui::Rect::from_min_max(
                egui::pos2(row.right() - 8.0 - width, row.top()),
                egui::pos2(row.right() - 8.0, row.bottom()),
            );
            widgets::paint_text(
                ui,
                detail_rect,
                Align2::RIGHT_CENTER,
                detail,
                theme::mono(11.0),
                palette.dim,
            );
            width + 12.0
        });
        let label_rect = egui::Rect::from_min_max(
            egui::pos2(x, row.top()),
            egui::pos2(row.right() - 8.0 - detail_width, row.bottom()),
        );
        let font = if self.strong {
            theme::medium(13.0)
        } else {
            egui::FontId::proportional(13.0)
        };
        let color = if self.selected || self.strong {
            palette.text
        } else {
            palette.text.gamma_multiply(0.92)
        };
        widgets::paint_text(ui, label_rect, Align2::LEFT_CENTER, self.label, font, color);
        (response, toggled)
    }
}

/// Draws the sidebar and returns what the user asked for.
#[hotpath::measure]
pub fn show(
    ui: &mut Ui,
    profiles: &[ConnectionProfile],
    folders: &[String],
    scripts: &[SavedScript],
    script_folders: &[ScriptFolder],
    explorer: &mut Explorer,
    actions: &mut Vec<Action>,
) {
    let palette = Palette::current(ui.ctx());
    ui.add_space(10.0);
    let header = ui.horizontal(|ui| {
        ui.add_space(14.0);
        widgets::caption(ui, "Connections");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add_space(8.0);
            if widgets::icon_button_sized(ui, Icon::Plus, "New connection", 24.0, 15.0).clicked() {
                actions.push(Action::NewConnection);
            }
            if widgets::icon_button_sized(ui, Icon::Folder, "New folder", 24.0, 15.0).clicked() {
                actions.push(new_folder(""));
            }
        });
    });
    let header = ui.interact(
        header.response.rect,
        ui.id().with("connections-header"),
        Sense::hover(),
    );
    drop_target(ui, &header, actions, to_root);
    ui.add_space(6.0);

    // Filter.
    ui.horizontal(|ui| {
        ui.add_space(12.0);
        let width = ui.available_width() - 12.0;
        egui::Frame::new()
            .fill(palette.surface)
            .stroke(egui::Stroke::new(1.0, palette.outline))
            .corner_radius(CornerRadius::same(7))
            .inner_margin(egui::Margin::symmetric(8, 0))
            .show(ui, |ui| {
                ui.set_width(width - 16.0);
                ui.horizontal(|ui| {
                    let (rect, _) = ui.allocate_exact_size(Vec2::splat(14.0), Sense::hover());
                    widgets::paint_icon(ui, Icon::Search, rect, palette.dim);
                    ui.add(
                        egui::TextEdit::singleline(&mut explorer.filter)
                            .hint_text("Filter objects")
                            .frame(egui::Frame::NONE)
                            .margin(egui::vec2(2.0, 6.0))
                            .desired_width(f32::INFINITY),
                    );
                });
            });
    });
    ui.add_space(8.0);

    if profiles.is_empty() && folders.is_empty() {
        ui.add_space(24.0);
        widgets::empty_state(
            ui,
            Icon::Database,
            "No connections yet",
            "Add one to start exploring.",
        );
        ui.add_space(12.0);
        ui.vertical_centered(|ui| {
            if widgets::button(
                ui,
                widgets::Tone::Primary,
                Some(Icon::Plus),
                "New connection",
            )
            .clicked()
            {
                actions.push(Action::NewConnection);
            }
        });
        return;
    }

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            let tree = Tree {
                profiles,
                folders,
                scripts,
                script_folders,
                filter: explorer.filter.trim().to_lowercase(),
            };
            tree.contents(ui, "", 0, explorer, actions);
            // The space below the tree takes the tree-wide menu.
            let rest = ui.available_size_before_wrap().max(Vec2::new(0.0, 12.0));
            let (_, response) = ui.allocate_exact_size(rest, Sense::click());
            drop_target(ui, &response, actions, to_root);
            response.context_menu(|ui| {
                if menu_item(ui, Icon::Plus, "New connection") {
                    actions.push(Action::NewConnection);
                }
                if menu_item(ui, Icon::Folder, "New folder…") {
                    actions.push(new_folder(""));
                }
            });
        });
    drag_preview(ui);
}

fn new_folder(parent: &str) -> Action {
    Action::AskRename(super::rename_flow::RenameTarget::NewFolder(
        parent.to_owned(),
    ))
}

/// The saved connections, filed in their folders.
struct Tree<'a> {
    profiles: &'a [ConnectionProfile],
    folders: &'a [String],
    scripts: &'a [SavedScript],
    script_folders: &'a [ScriptFolder],
    /// The filter box, lower case.
    filter: String,
}

impl Tree<'_> {
    /// Whether a connection shows while filtering: its name matches, or it
    /// is online and may hold objects that do.
    fn shown(&self, profile: &ConnectionProfile, explorer: &Explorer) -> bool {
        self.filter.is_empty()
            || matches_filter(&profile.display_name(), &self.filter)
            || explorer.link(profile.id).is_online()
    }

    /// The connections in `folder` and the folders below it that show.
    fn count(&self, folder: &str, explorer: &Explorer) -> usize {
        let prefix = format!("{folder}/");
        self.profiles
            .iter()
            .filter(|profile| profile.folder == folder || profile.folder.starts_with(&prefix))
            .filter(|profile| self.shown(profile, explorer))
            .count()
    }

    /// The folders in `parent`, then its connections, at `depth`.
    fn contents(
        &self,
        ui: &mut Ui,
        parent: &str,
        depth: usize,
        explorer: &mut Explorer,
        actions: &mut Vec<Action>,
    ) {
        let filtering = !self.filter.is_empty();
        for folder in self
            .folders
            .iter()
            .filter(|folder| violet_app::parent_folder(folder) == parent)
        {
            let count = self.count(folder, explorer);
            if filtering && count == 0 {
                continue;
            }
            let key = NodeKey::ConnectionFolder(folder.clone());
            let expanded = filtering || explorer.is_expanded(&key);
            let name = folder.rsplit('/').next().unwrap_or(folder);
            let detail = count.to_string();
            let mut row = Row::new(
                depth,
                if expanded {
                    Icon::FolderOpen
                } else {
                    Icon::Folder
                },
                name,
            );
            row.expanded = Some(expanded);
            row.selected = explorer.selected.as_ref() == Some(&key);
            row.detail = Some(&detail);
            row.draggable = true;
            let response = row.show(ui).on_hover_text(folder);
            response.dnd_set_drag_payload(DragItem::Folder(folder.clone()));
            drop_target(ui, &response, actions, |item| to_folder(item, folder));
            if response.clicked() {
                explorer.pick(key.clone());
                explorer.toggle(key);
            }
            response.context_menu(|ui| {
                if menu_item(ui, Icon::Folder, "New subfolder…") {
                    actions.push(new_folder(folder));
                }
                if menu_item(ui, Icon::Pencil, "Rename…") {
                    actions.push(Action::AskRename(super::rename_flow::RenameTarget::Folder(
                        folder.clone(),
                    )));
                }
                if menu_item(ui, Icon::Trash, "Delete") {
                    actions.push(Action::DeleteFolder(folder.clone()));
                }
            });
            if expanded {
                self.contents(ui, folder, depth + 1, explorer, actions);
            }
        }
        for profile in self
            .profiles
            .iter()
            .filter(|profile| profile.folder == parent)
        {
            if !self.shown(profile, explorer) {
                continue;
            }
            // The connection's own rows count depth from 0: the frame
            // shifts them under their folder.
            let indent = (depth as f32 * INDENT).min(f32::from(i8::MAX)) as i8;
            egui::Frame::new()
                .inner_margin(egui::Margin {
                    left: indent,
                    ..egui::Margin::ZERO
                })
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    let scripts = Scripts {
                        all: self
                            .scripts
                            .iter()
                            .filter(|script| script.connection == profile.id)
                            .collect(),
                        folders: self
                            .script_folders
                            .iter()
                            .filter(|folder| folder.connection == profile.id)
                            .collect(),
                    };
                    connection(ui, profile, self.folders, &scripts, explorer, actions);
                });
        }
    }
}

fn connection(
    ui: &mut Ui,
    profile: &ConnectionProfile,
    all_folders: &[String],
    scripts: &Scripts<'_>,
    explorer: &mut Explorer,
    actions: &mut Vec<Action>,
) {
    let palette = Palette::current(ui.ctx());
    let id = profile.id;
    let family = Family::of(profile.driver.as_str());
    let language = explorer.language(profile.driver.as_str());
    let key = NodeKey::Connection(id);
    let link = explorer.link(id).clone();
    let expanded = explorer.is_expanded(&key) && link.is_online();
    let name = profile.display_name();
    let detail = profile.address();
    let status = match &link {
        Link::Online(_) => palette.success,
        Link::Connecting => palette.warning,
        Link::Failed(_) => palette.danger,
        Link::Offline => palette.dim.gamma_multiply(0.6),
    };

    let mut row = Row::new(0, Icon::Database, &name);
    row.expanded = Some(expanded);
    row.status = Some(status);
    row.icon_tint = profile.color.map(theme::tag);
    row.selected = explorer.selected.as_ref() == Some(&key);
    row.loading = matches!(link, Link::Connecting);
    row.strong = true;
    row.detail = Some(&detail);
    row.draggable = true;
    let response = row.show(ui);
    response.dnd_set_drag_payload(DragItem::Connection {
        id,
        folder: profile.folder.clone(),
        name: name.clone(),
    });
    // Dropped on a connection, an item joins it in its folder.
    drop_target(ui, &response, actions, |item| {
        to_folder(item, &profile.folder)
    });
    let response = match &link {
        Link::Online(server) => response.on_hover_text(format!(
            "{} · {} as {}",
            server.version, server.database, server.user
        )),
        Link::Failed(error) => response.on_hover_text(error),
        _ => response,
    };

    if response.clicked() {
        explorer.pick(key.clone());
        match link {
            Link::Online(_) => {
                explorer.toggle(key.clone());
            }
            Link::Offline | Link::Failed(_) => {
                explorer.expanded.insert(key.clone());
                actions.push(Action::Connect(id));
            }
            Link::Connecting => {}
        }
    }
    if response.double_clicked() && link.is_online() {
        actions.push(Action::OpenEditor {
            connection: Some(id),
            sql: String::new(),
        });
    }
    response.context_menu(|ui| {
        if link.is_online() {
            if menu_item(ui, Icon::Terminal, crate::lang::editor_name(language)) {
                actions.push(Action::OpenEditor {
                    connection: Some(id),
                    sql: String::new(),
                });
            }
            if menu_item(ui, Icon::Refresh, "Refresh") {
                actions.push(Action::Refresh(id));
            }
            if matches!(family, Family::Postgres | Family::MySql)
                && menu_item(ui, Icon::Plus, "New database…")
            {
                actions.push(Action::AskRename(
                    super::rename_flow::RenameTarget::NewDatabase(id),
                ));
            }
            if language == Language::Redis || profile.show_all_databases {
                if menu_item(ui, Icon::Funnel, "Choose databases…") {
                    actions.push(Action::ChooseDatabases(id));
                }
                if explorer
                    .database_filters
                    .contains_key(&Explorer::database_scope(id))
                    && menu_item(ui, Icon::Close, "Show all databases")
                {
                    actions.push(Action::SetTreeFilter(
                        TreeFilter::Databases,
                        Explorer::database_scope(id),
                        None,
                    ));
                }
            }
            if menu_item(ui, Icon::Power, "Disconnect") {
                actions.push(Action::Disconnect(id));
            }
        } else if menu_item(ui, Icon::Plug, "Connect") {
            explorer.expanded.insert(key.clone());
            actions.push(Action::Connect(id));
        }
        ui.separator();
        if menu_item(ui, Icon::Archive, "Backup…") {
            actions.push(Action::OpenTask {
                connection: id,
                kind: TaskKind::Backup,
                objects: Vec::new(),
            });
        }
        // Redis restores an RDB file on the server's side, not over a
        // connection.
        if language != Language::Redis && menu_item(ui, Icon::ArchiveRestore, "Restore…") {
            actions.push(Action::OpenTask {
                connection: id,
                kind: TaskKind::Restore,
                objects: Vec::new(),
            });
        }
        ui.separator();
        if menu_item(ui, Icon::Pencil, "Edit connection…") {
            actions.push(Action::EditConnection(id));
        }
        if menu_item(ui, Icon::Copy, "Duplicate") {
            actions.push(Action::DuplicateConnection(id));
        }
        ui.menu_button("Move to folder", |ui| {
            if !profile.folder.is_empty() && menu_item(ui, Icon::Database, "(root)") {
                actions.push(Action::MoveToFolder(id, String::new()));
            }
            for folder in all_folders
                .iter()
                .filter(|folder| **folder != profile.folder)
            {
                if menu_item(ui, Icon::Folder, folder) {
                    actions.push(Action::MoveToFolder(id, folder.clone()));
                }
            }
            if menu_item(ui, Icon::Plus, "New folder…") {
                actions.push(new_folder(""));
            }
        });
        if menu_item(ui, Icon::Trash, "Delete…") {
            actions.push(Action::AskDeleteConnection(id));
        }
    });

    if !expanded {
        return;
    }
    if !scripts.all.is_empty() || !scripts.folders.is_empty() {
        scripts_node(ui, profile, scripts, explorer, actions);
    }
    if language == Language::Redis {
        match explorer.schemas.get(&id).cloned().unwrap_or_default() {
            Loadable::Idle | Loadable::Loading => loading_row(ui, 1),
            Loadable::Failed(error) => error_row(ui, 1, &error),
            Loadable::Ready(databases) => {
                for database in picked_databases(explorer, id, &databases) {
                    keyspace_node(ui, id, database, explorer, actions);
                }
            }
        }
        return;
    }
    // Every database on the server, when the connection lists them; else
    // its own database's schemas straight away.
    if profile.show_all_databases
        && let Some(databases) = explorer.databases.get(&id).cloned()
    {
        match databases {
            Loadable::Idle | Loadable::Loading => loading_row(ui, 1),
            Loadable::Failed(error) => error_row(ui, 1, &error),
            Loadable::Ready(databases) => {
                let picked = explorer
                    .database_filters
                    .get(&Explorer::database_scope(id))
                    .cloned();
                let shown =
                    |database: &&String| picked.as_ref().is_none_or(|p| p.contains(*database));
                for database in databases.iter().filter(shown) {
                    database_node(ui, id, family, database, explorer, actions);
                }
                // Hidden ones are a click away.
                let hidden = databases.len() - databases.iter().filter(shown).count();
                if hidden > 0 {
                    let palette = Palette::current(ui.ctx());
                    let label = format!(
                        "{hidden} hidden {}",
                        if hidden == 1 { "database" } else { "databases" }
                    );
                    let mut row = Row::new(1, Icon::Funnel, &label);
                    row.icon_tint = Some(palette.dim);
                    if row
                        .show(ui)
                        .on_hover_text("Choose the databases to show")
                        .clicked()
                    {
                        actions.push(Action::ChooseDatabases(id));
                    }
                }
            }
        }
        return;
    }
    folders(ui, id, family, 1, explorer, actions);
}

/// The databases of a connection the user picked to see, all when none
/// were. The connection's own is always among them.
fn picked_databases<'a>(
    explorer: &Explorer,
    id: ConnectionId,
    all: &'a [String],
) -> Vec<&'a String> {
    let picked = explorer.database_filters.get(&Explorer::database_scope(id));
    let own = match explorer.link(id) {
        Link::Online(server) => Some(server.database.as_str()),
        _ => None,
    };
    all.iter()
        .filter(|name| {
            picked.is_none_or(|picked| picked.contains(*name)) || own == Some(name.as_str())
        })
        .collect()
}

/// A connection's saved scripts and the folders they are kept in.
struct Scripts<'a> {
    all: Vec<&'a SavedScript>,
    folders: Vec<&'a ScriptFolder>,
}

/// The scripts saved for a connection, each opening on its database.
fn scripts_node(
    ui: &mut Ui,
    profile: &ConnectionProfile,
    scripts: &Scripts<'_>,
    explorer: &mut Explorer,
    actions: &mut Vec<Action>,
) {
    let key = NodeKey::Scripts(profile.id);
    let filter = explorer.filter.trim().to_lowercase();
    let filtering = !filter.is_empty();
    let shown: Vec<&SavedScript> = scripts
        .all
        .iter()
        .copied()
        .filter(|script| !filtering || matches_filter(&script.name, &filter))
        .collect();
    if filtering && shown.is_empty() {
        return;
    }
    // The database shows only for scripts of another one than the
    // connection's.
    let own = match explorer.link(profile.id) {
        Link::Online(server) => server.database.clone(),
        _ => profile.database.clone(),
    };
    let expanded = filtering || explorer.is_expanded(&key);
    let count = shown.len().to_string();
    let mut row = Row::new(1, Icon::Folder, "Scripts");
    row.expanded = Some(expanded);
    row.detail = Some(&count);
    let response = row.show(ui);
    drop_target(ui, &response, actions, |item| {
        to_script_folder(item, profile.id, None, "")
    });
    if response.clicked() {
        explorer.toggle(key);
    }
    response.context_menu(|ui| {
        if !own.is_empty() && menu_item(ui, Icon::Folder, "New folder…") {
            actions.push(new_script_folder(profile.id, &own, ""));
        }
    });
    if !expanded {
        return;
    }
    let level = ScriptLevel {
        connection: profile.id,
        own: &own,
        shown: &shown,
        folders: &scripts.folders,
        filtering,
    };
    level.show(ui, None, "", 2, explorer, actions);
}

fn new_script_folder(connection: ConnectionId, database: &str, parent: &str) -> Action {
    Action::AskRename(super::rename_flow::RenameTarget::NewScriptFolder {
        connection,
        database: database.to_owned(),
        parent: parent.to_owned(),
    })
}

/// What drawing a level of a connection's scripts needs.
struct ScriptLevel<'a> {
    connection: ConnectionId,
    /// The connection's own database.
    own: &'a str,
    /// The scripts the filter lets through.
    shown: &'a [&'a SavedScript],
    folders: &'a [&'a ScriptFolder],
    filtering: bool,
}

impl ScriptLevel<'_> {
    /// The folders, then the scripts, in `folder` of `database` (of every
    /// database at the top).
    fn show(
        &self,
        ui: &mut Ui,
        database: Option<&str>,
        folder: &str,
        depth: usize,
        explorer: &mut Explorer,
        actions: &mut Vec<Action>,
    ) {
        let here = |db: &str| database.is_none_or(|database| database == db);
        for child in self.folders.iter().filter(|child| {
            here(&child.database) && violet_app::parent_folder(&child.folder) == folder
        }) {
            let prefix = format!("{}/", child.folder);
            let count = self
                .shown
                .iter()
                .filter(|script| {
                    script.database == child.database
                        && (script.folder == child.folder || script.folder.starts_with(&prefix))
                })
                .count();
            if self.filtering && count == 0 {
                continue;
            }
            let key = NodeKey::ScriptFolder(
                self.connection,
                child.database.clone(),
                child.folder.clone(),
            );
            let expanded = self.filtering || explorer.is_expanded(&key);
            let name = child.folder.rsplit('/').next().unwrap_or(&child.folder);
            let detail = if child.database == self.own {
                count.to_string()
            } else {
                format!("{} · {count}", child.database)
            };
            let mut row = Row::new(
                depth,
                if expanded {
                    Icon::FolderOpen
                } else {
                    Icon::Folder
                },
                name,
            );
            row.expanded = Some(expanded);
            row.detail = Some(&detail);
            row.draggable = true;
            let response = row.show(ui);
            response.dnd_set_drag_payload(DragItem::ScriptFolder {
                connection: self.connection,
                database: child.database.clone(),
                folder: child.folder.clone(),
            });
            drop_target(ui, &response, actions, |item| {
                to_script_folder(item, self.connection, Some(&child.database), &child.folder)
            });
            if response.clicked() {
                explorer.toggle(key);
            }
            response.context_menu(|ui| {
                if menu_item(ui, Icon::Folder, "New folder…") {
                    actions.push(new_script_folder(
                        self.connection,
                        &child.database,
                        &child.folder,
                    ));
                }
            });
            if expanded {
                self.show(
                    ui,
                    Some(&child.database),
                    &child.folder,
                    depth + 1,
                    explorer,
                    actions,
                );
            }
        }
        for script in self
            .shown
            .iter()
            .filter(|script| here(&script.database) && script.folder == folder)
        {
            let mut row = Row::new(depth, Icon::FileCode, &script.name);
            if script.database != self.own {
                row.detail = Some(&script.database);
            }
            row.draggable = true;
            let response = row.show(ui).on_hover_text(format!(
                "{}\n{}",
                script.database,
                script.path.display()
            ));
            response.dnd_set_drag_payload(DragItem::Script {
                path: script.path.clone(),
                connection: script.connection,
                database: script.database.clone(),
                folder: script.folder.clone(),
                name: script.name.clone(),
            });
            if response.clicked() {
                actions.push(Action::OpenScript(script.path.clone()));
            }
            response.context_menu(|ui| {
                if menu_item(ui, Icon::FileCode, "Open") {
                    actions.push(Action::OpenScript(script.path.clone()));
                }
                ui.menu_button("Move to folder", |ui| {
                    if !script.folder.is_empty() && menu_item(ui, Icon::Folder, "(top level)") {
                        actions.push(Action::MoveScript(script.path.clone(), String::new()));
                    }
                    for target in self.folders.iter().filter(|target| {
                        target.database == script.database && target.folder != script.folder
                    }) {
                        if menu_item(ui, Icon::Folder, &target.folder) {
                            actions.push(Action::MoveScript(
                                script.path.clone(),
                                target.folder.clone(),
                            ));
                        }
                    }
                    if menu_item(ui, Icon::Plus, "New folder…") {
                        actions.push(new_script_folder(
                            self.connection,
                            &script.database,
                            &script.folder,
                        ));
                    }
                });
                if menu_item(ui, Icon::FolderOpen, "Show in folder") {
                    actions.push(Action::Reveal(script.path.clone()));
                }
                if menu_item(ui, Icon::Copy, "Copy path") {
                    actions.push(Action::Copy(script.path.display().to_string()));
                }
            });
        }
    }
}

/// The folders of session `id`'s database, at `depth`, as DBeaver lays
/// them out: schemas, then triggers, roles and extensions where the
/// database has them.
fn folders(
    ui: &mut Ui,
    id: ConnectionId,
    family: Family,
    depth: usize,
    explorer: &mut Explorer,
    actions: &mut Vec<Action>,
) {
    for folder in catalog_flow::folders(family) {
        if *folder == Folder::Schemas && family == Family::MySql {
            databases_inline(ui, id, depth, explorer, actions);
        } else if *folder == Folder::Schemas {
            schemas_folder(ui, id, depth, explorer, actions);
        } else {
            folder_node(ui, id, *folder, depth, explorer, actions);
        }
    }
}

/// MySQL's schemas are its databases: the session's own one shows its
/// objects straight away; without one, every database shows.
fn databases_inline(
    ui: &mut Ui,
    id: ConnectionId,
    depth: usize,
    explorer: &mut Explorer,
    actions: &mut Vec<Action>,
) {
    match explorer.schemas.get(&id).cloned().unwrap_or_default() {
        Loadable::Idle | Loadable::Loading => loading_row(ui, depth),
        Loadable::Failed(error) => error_row(ui, depth, &error),
        Loadable::Ready(schemas) => match &schemas[..] {
            [database] => {
                if matches!(
                    explorer.objects.get(&(id, database.clone())),
                    None | Some(Loadable::Idle)
                ) {
                    actions.push(Action::LoadObjects(id, database.clone()));
                }
                schema_contents(ui, id, database, depth, explorer, actions);
            }
            schemas => {
                for schema in schemas {
                    schema_node(ui, id, schema, Icon::Database, depth, explorer, actions);
                }
            }
        },
    }
}

/// The schemas folder: the schemas picked for the database, or all.
fn schemas_folder(
    ui: &mut Ui,
    id: ConnectionId,
    depth: usize,
    explorer: &mut Explorer,
    actions: &mut Vec<Action>,
) {
    let key = NodeKey::Folder(id, Folder::Schemas);
    let filtering = !explorer.filter.trim().is_empty();
    let expanded = filtering || explorer.is_expanded(&key);
    let all = explorer.schemas.get(&id).cloned().unwrap_or_default();
    let picked = explorer
        .schema_scope(id)
        .and_then(|scope| explorer.schema_filters.get(&scope).cloned());
    let detail = match (&all, &picked) {
        (Loadable::Ready(all), Some(picked)) => Some(format!(
            "{} of {}",
            all.iter().filter(|s| picked.contains(*s)).count(),
            all.len()
        )),
        (Loadable::Ready(all), None) => Some(all.len().to_string()),
        _ => None,
    };
    let mut row = Row::new(depth, Icon::Folder, Folder::Schemas.label());
    row.expanded = Some(expanded);
    row.selected = explorer.selected.as_ref() == Some(&key);
    row.detail = detail.as_deref();
    let response = row.show(ui);
    let response = if picked.is_some() {
        response.on_hover_text("Showing the schemas picked for this database")
    } else {
        response
    };
    if response.clicked() {
        explorer.pick(key.clone());
        explorer.toggle(key);
    }
    response.context_menu(|ui| {
        if menu_item(ui, Icon::Funnel, "Choose schemas…") {
            actions.push(Action::ChooseSchemas(id));
        }
        if picked.is_some()
            && let Some(scope) = explorer.schema_scope(id)
            && menu_item(ui, Icon::Close, "Show all schemas")
        {
            actions.push(Action::SetTreeFilter(TreeFilter::Schemas, scope, None));
        }
        if menu_item(ui, Icon::Refresh, "Refresh") {
            actions.push(Action::Refresh(id));
        }
    });
    if !expanded {
        return;
    }
    match all {
        Loadable::Idle | Loadable::Loading => loading_row(ui, depth + 1),
        Loadable::Failed(error) => error_row(ui, depth + 1, &error),
        Loadable::Ready(schemas) => {
            let shown = schemas.iter().filter(|schema| {
                picked
                    .as_ref()
                    .is_none_or(|picked| picked.contains(*schema))
            });
            for schema in shown {
                schema_node(ui, id, schema, Icon::Schema, depth + 1, explorer, actions);
            }
        }
    }
}

/// A database of connection `parent`. Its own database is browsed on the
/// connection's session; any other opens a session of its own.
fn database_node(
    ui: &mut Ui,
    parent: ConnectionId,
    family: Family,
    database: &str,
    explorer: &mut Explorer,
    actions: &mut Vec<Action>,
) {
    let palette = Palette::current(ui.ctx());
    let key = NodeKey::Database(parent, database.to_owned());
    let own = matches!(
        explorer.link(parent),
        Link::Online(server) if server.database == database
    );
    let session = if own {
        Some(parent)
    } else {
        explorer.opened(parent, database)
    };
    let link = session.map_or(Link::Offline, |id| explorer.link(id).clone());
    let expanded = explorer.is_expanded(&key);

    let mut row = Row::new(1, Icon::Database, database);
    row.expanded = Some(expanded);
    row.selected = explorer.selected.as_ref() == Some(&key);
    row.loading = matches!(link, Link::Connecting);
    row.strong = own;
    row.status = Some(match &link {
        Link::Online(_) => palette.success,
        Link::Connecting => palette.warning,
        Link::Failed(_) => palette.danger,
        Link::Offline => palette.dim.gamma_multiply(0.6),
    });
    let response = row.show(ui);
    let response = match &link {
        Link::Failed(error) => response.on_hover_text(error),
        _ => response,
    };
    if response.clicked() {
        explorer.pick(key.clone());
        if expanded && link.is_online() {
            explorer.toggle(key.clone());
        } else {
            actions.push(Action::OpenDatabase(parent, database.to_owned()));
        }
    }
    response.context_menu(|ui| {
        match session.filter(|_| link.is_online()) {
            Some(session) => {
                if menu_item(ui, Icon::Terminal, "New SQL editor") {
                    actions.push(Action::OpenEditor {
                        connection: Some(session),
                        sql: String::new(),
                    });
                }
                if menu_item(ui, Icon::Refresh, "Refresh") {
                    actions.push(Action::Refresh(session));
                }
                if !own && menu_item(ui, Icon::Power, "Disconnect") {
                    actions.push(Action::Disconnect(session));
                }
            }
            None => {
                if menu_item(ui, Icon::Plug, "Connect") {
                    actions.push(Action::OpenDatabase(parent, database.to_owned()));
                }
            }
        }
        ui.separator();
        if menu_item(ui, Icon::Archive, "Backup…") {
            actions.push(Action::DatabaseTask(
                parent,
                database.to_owned(),
                TaskKind::Backup,
            ));
        }
        if menu_item(ui, Icon::ArchiveRestore, "Restore…") {
            actions.push(Action::DatabaseTask(
                parent,
                database.to_owned(),
                TaskKind::Restore,
            ));
        }
        ui.separator();
        if family == Family::Postgres && menu_item(ui, Icon::Pencil, "Rename…  (F2)") {
            actions.push(Action::AskRename(
                super::rename_flow::RenameTarget::Database {
                    connection: parent,
                    name: database.to_owned(),
                },
            ));
        }
        if menu_item(ui, Icon::Copy, "Copy name") {
            actions.push(Action::Copy(database.to_owned()));
        }
        if matches!(family, Family::Postgres | Family::MySql)
            && !own
            && menu_item(ui, Icon::Trash, "Delete database…  (Del)")
        {
            actions.push(Action::AskDropDatabase(parent, database.to_owned()));
        }
    });

    if !expanded {
        return;
    }
    match (session, &link) {
        (Some(session), Link::Online(_)) => folders(ui, session, family, 2, explorer, actions),
        (_, Link::Failed(error)) => error_row(ui, 2, error),
        (_, Link::Connecting) => loading_row(ui, 2),
        // Closed since it was opened: a click opens it again.
        _ => {}
    }
}

/// The triggers, roles or extensions folder, listed the first time it
/// opens.
fn folder_node(
    ui: &mut Ui,
    id: ConnectionId,
    folder: Folder,
    depth: usize,
    explorer: &mut Explorer,
    actions: &mut Vec<Action>,
) {
    let key = NodeKey::Folder(id, folder);
    let filter = explorer.filter.trim().to_lowercase();
    let items = explorer
        .folders
        .get(&(id, folder))
        .cloned()
        .unwrap_or_default();
    // While filtering, a folder shows only when something in it matches.
    if !filter.is_empty() {
        let matches = matches!(&items, Loadable::Ready(items)
            if items.iter().any(|item| matches_filter(&item.name, &filter)));
        if !matches {
            return;
        }
    }
    let expanded = !filter.is_empty() || explorer.is_expanded(&key);
    let count = match &items {
        Loadable::Ready(items) => Some(items.len().to_string()),
        _ => None,
    };
    let mut row = Row::new(depth, Icon::Folder, folder.label());
    row.expanded = Some(expanded);
    row.selected = explorer.selected.as_ref() == Some(&key);
    row.loading = items.is_loading();
    row.detail = count.as_deref();
    let response = row.show(ui);
    if response.clicked() {
        explorer.pick(key.clone());
        if explorer.toggle(key) && matches!(items, Loadable::Idle | Loadable::Failed(_)) {
            actions.push(Action::LoadFolder(id, folder));
        }
    }
    response.context_menu(|ui| {
        if menu_item(ui, Icon::Refresh, "Refresh") {
            actions.push(Action::LoadFolder(id, folder));
        }
    });
    if !expanded {
        return;
    }
    let icon = match folder {
        Folder::Triggers => Icon::Zap,
        Folder::Roles => Icon::Lock,
        Folder::Extensions => Icon::Package,
        Folder::Schemas => Icon::Schema,
    };
    match items {
        Loadable::Idle | Loadable::Loading => loading_row(ui, depth + 1),
        Loadable::Failed(error) => error_row(ui, depth + 1, &error),
        Loadable::Ready(items) if items.is_empty() => {
            let palette = Palette::current(ui.ctx());
            let mut row = Row::new(depth + 1, Icon::Info, "None");
            row.icon_tint = Some(palette.dim);
            row.show(ui);
        }
        Loadable::Ready(items) => {
            for item in items.iter() {
                if !filter.is_empty() && !matches_filter(&item.name, &filter) {
                    continue;
                }
                folder_item(ui, id, folder, icon, item, depth + 1, actions);
            }
        }
    }
}

/// A trigger, role or extension. A trigger opens its definition.
fn folder_item(
    ui: &mut Ui,
    id: ConnectionId,
    folder: Folder,
    icon: Icon,
    item: &FolderItem,
    depth: usize,
    actions: &mut Vec<Action>,
) {
    let mut row = Row::new(depth, icon, &item.name);
    row.detail = item.detail.as_deref();
    let response = row.show(ui);
    // A trigger knows its table, which reading its definition needs.
    let trigger = match (folder, &item.schema, &item.table) {
        (Folder::Triggers, Some(schema), Some(table)) => Some((schema, table)),
        _ => None,
    };
    let open = |(schema, table): (&String, &String)| Action::OpenTrigger {
        connection: id,
        schema: schema.clone(),
        table: table.clone(),
        name: item.name.clone(),
    };
    let response = match (&item.schema, &item.table) {
        (Some(schema), Some(table)) => response.on_hover_text(format!("On {schema}.{table}")),
        _ => response,
    };
    if response.clicked()
        && let Some(trigger) = trigger
    {
        actions.push(open(trigger));
    }
    response.context_menu(|ui| {
        if let Some(trigger) = trigger
            && menu_item(ui, Icon::FileCode, "Open definition")
        {
            actions.push(open(trigger));
        }
        if menu_item(ui, Icon::Copy, "Copy name") {
            actions.push(Action::Copy(item.name.clone()));
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn schema_node(
    ui: &mut Ui,
    id: violet_core::ConnectionId,
    schema: &str,
    icon: Icon,
    depth: usize,
    explorer: &mut Explorer,
    actions: &mut Vec<Action>,
) {
    let key = NodeKey::Schema(id, schema.to_owned());
    let expanded = explorer.is_expanded(&key);
    let filter = explorer.filter.trim().to_lowercase();
    let objects = explorer
        .objects
        .get(&(id, schema.to_owned()))
        .cloned()
        .unwrap_or_default();

    // While filtering, hide loaded schemas with no match.
    if !filter.is_empty()
        && let Loadable::Ready(objects) = &objects
        && !objects
            .iter()
            .any(|object| matches_filter(&object.name, &filter))
    {
        return;
    }

    let mut row = Row::new(depth, icon, schema);
    row.expanded = Some(expanded);
    row.selected = explorer.selected.as_ref() == Some(&key);
    row.loading = objects.is_loading();
    let response = row.show(ui);
    if response.clicked() {
        explorer.pick(key.clone());
        if explorer.toggle(key.clone()) && matches!(objects, Loadable::Idle | Loadable::Failed(_)) {
            actions.push(Action::LoadObjects(id, schema.to_owned()));
        }
    }
    response.context_menu(|ui| {
        if menu_item(ui, Icon::Terminal, "New SQL editor") {
            actions.push(Action::OpenEditor {
                connection: Some(id),
                sql: format!("SET search_path TO {};\n\n", quote(schema)),
            });
        }
        if menu_item(ui, Icon::Refresh, "Refresh") {
            actions.push(Action::LoadObjects(id, schema.to_owned()));
        }
        if menu_item(ui, Icon::Pencil, "Rename…  (F2)") {
            actions.push(Action::AskRename(
                super::rename_flow::RenameTarget::Schema {
                    connection: id,
                    name: schema.to_owned(),
                },
            ));
        }
        if menu_item(ui, Icon::Copy, "Copy name") {
            actions.push(Action::Copy(schema.to_owned()));
        }
        ui.separator();
        if menu_item(ui, Icon::Archive, "Backup schema…") {
            actions.push(Action::OpenTask {
                connection: id,
                kind: TaskKind::Backup,
                objects: vec![ObjectRef {
                    schema: schema.to_owned(),
                    name: None,
                }],
            });
        }
        if menu_item(ui, Icon::ArchiveRestore, "Restore…") {
            actions.push(Action::OpenTask {
                connection: id,
                kind: TaskKind::Restore,
                objects: Vec::new(),
            });
        }
    });
    if expanded {
        schema_contents(ui, id, schema, depth + 1, explorer, actions);
    }
}

/// A schema's objects in their groups, at `depth`.
fn schema_contents(
    ui: &mut Ui,
    id: violet_core::ConnectionId,
    schema: &str,
    depth: usize,
    explorer: &mut Explorer,
    actions: &mut Vec<Action>,
) {
    let filter = explorer.filter.trim().to_lowercase();
    let objects = explorer
        .objects
        .get(&(id, schema.to_owned()))
        .cloned()
        .unwrap_or_default();
    match objects {
        Loadable::Idle | Loadable::Loading => loading_row(ui, depth),
        Loadable::Failed(error) => error_row(ui, depth, &error),
        Loadable::Ready(objects) => {
            // One pass sorts the objects into their groups.
            let mut groups: [Vec<&DbObject>; ObjectKind::ALL.len()] = Default::default();
            for object in objects.iter() {
                if filter.is_empty() || matches_filter(&object.name, &filter) {
                    let group = ObjectKind::ALL
                        .iter()
                        .position(|kind| *kind == object.kind)
                        .unwrap_or_default();
                    groups[group].push(object);
                }
            }
            for (kind, members) in ObjectKind::ALL.into_iter().zip(&groups) {
                if members.is_empty() {
                    continue;
                }
                group_node(
                    ui,
                    id,
                    schema,
                    depth,
                    kind,
                    members,
                    explorer,
                    actions,
                    !filter.is_empty(),
                );
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn group_node(
    ui: &mut Ui,
    id: violet_core::ConnectionId,
    schema: &str,
    depth: usize,
    kind: ObjectKind,
    members: &[&DbObject],
    explorer: &mut Explorer,
    actions: &mut Vec<Action>,
    filtering: bool,
) {
    let key = NodeKey::Group(id, schema.to_owned(), kind);
    let expanded = filtering || explorer.is_expanded(&key);
    let count = members.len().to_string();
    let mut row = Row::new(depth, Icon::Folder, kind.plural());
    row.expanded = Some(expanded);
    row.detail = Some(&count);
    if row.show(ui).clicked() {
        explorer.toggle(key);
    }
    if !expanded {
        return;
    }
    for object in members {
        object_node(ui, id, object, depth + 1, explorer, actions);
    }
}

fn object_node(
    ui: &mut Ui,
    id: violet_core::ConnectionId,
    object: &DbObject,
    depth: usize,
    explorer: &mut Explorer,
    actions: &mut Vec<Action>,
) {
    let key = NodeKey::Object(id, object.schema.clone(), object.name.clone());
    let relation = object.kind.is_relation();
    let expanded = relation && explorer.is_expanded(&key);
    let columns = if relation {
        explorer
            .columns
            .get(&(id, object.schema.clone(), object.name.clone()))
            .cloned()
            .unwrap_or_default()
    } else {
        Loadable::Idle
    };
    let detail = object
        .estimated_rows
        .filter(|_| relation)
        .map(compact_count);

    let mut row = Row::new(depth, Icon::for_kind(object.kind), &object.name);
    row.expanded = relation.then_some(expanded);
    row.selected = explorer.selected.as_ref() == Some(&key);
    row.loading = columns.is_loading();
    row.detail = detail.as_deref();
    let (mut response, toggled) = row.show_with_toggle(ui);
    if let Some(comment) = &object.comment {
        response = response.on_hover_text(comment);
    }

    // As in DBeaver: clicking a table opens its data; the chevron shows
    // its columns.
    if response.clicked() {
        explorer.pick(key.clone());
        if toggled {
            if explorer.toggle(key.clone())
                && matches!(columns, Loadable::Idle | Loadable::Failed(_))
            {
                actions.push(Action::LoadColumns(
                    id,
                    object.schema.clone(),
                    object.name.clone(),
                ));
            }
        } else if relation {
            actions.push(Action::OpenData {
                connection: id,
                schema: object.schema.clone(),
                relation: object.name.clone(),
                subtype: None,
            });
        } else if object.kind == ObjectKind::Function {
            actions.push(Action::OpenDefinition {
                connection: id,
                schema: object.schema.clone(),
                name: object.name.clone(),
                kind: object.kind,
            });
        }
    }
    response.context_menu(|ui| {
        if relation {
            if menu_item(ui, Icon::Rows, "View data") {
                actions.push(Action::OpenData {
                    connection: id,
                    schema: object.schema.clone(),
                    relation: object.name.clone(),
                    subtype: None,
                });
            }
            if menu_item(ui, Icon::Terminal, "New SQL editor") {
                actions.push(Action::OpenEditor {
                    connection: Some(id),
                    sql: format!(
                        "SELECT *\nFROM {}.{}\nLIMIT 100;\n",
                        quote(&object.schema),
                        quote(&object.name)
                    ),
                });
            }
        }
        let has_source = matches!(
            object.kind,
            ObjectKind::Function | ObjectKind::View | ObjectKind::MaterializedView
        );
        if has_source && menu_item(ui, Icon::FileCode, "Open definition") {
            actions.push(Action::OpenDefinition {
                connection: id,
                schema: object.schema.clone(),
                name: object.name.clone(),
                kind: object.kind,
            });
        }
        if menu_item(ui, Icon::Copy, "Copy name") {
            actions.push(Action::Copy(object.name.clone()));
        }
        if menu_item(ui, Icon::Copy, "Copy qualified name") {
            actions.push(Action::Copy(format!(
                "{}.{}",
                quote(&object.schema),
                quote(&object.name)
            )));
        }
        if menu_item(ui, Icon::Pencil, "Rename…  (F2)") {
            actions.push(Action::AskRename(
                super::rename_flow::RenameTarget::Relation {
                    connection: id,
                    schema: object.schema.clone(),
                    name: object.name.clone(),
                },
            ));
        }
        if relation {
            ui.separator();
            if menu_item(ui, Icon::Archive, "Backup table…") {
                actions.push(Action::OpenTask {
                    connection: id,
                    kind: TaskKind::Backup,
                    objects: vec![ObjectRef {
                        schema: object.schema.clone(),
                        name: Some(object.name.clone()),
                    }],
                });
            }
        }
    });

    if !expanded {
        return;
    }
    match columns {
        Loadable::Idle | Loadable::Loading => loading_row(ui, depth + 1),
        Loadable::Failed(error) => error_row(ui, depth + 1, &error),
        Loadable::Ready(columns) => {
            for column in columns.iter() {
                let palette = Palette::current(ui.ctx());
                let icon = if column.primary_key {
                    Icon::Key
                } else {
                    Icon::Columns
                };
                let mut row = Row::new(depth + 1, icon, &column.name);
                row.detail = Some(&column.data_type);
                if column.primary_key {
                    row.icon_tint = Some(palette.warning);
                }
                // The tooltip text is built only while it shows.
                let response = row.show(ui).on_hover_ui(|ui| {
                    let mut tip = format!("{} {}", column.name, column.data_type);
                    if !column.nullable {
                        tip.push_str(" NOT NULL");
                    }
                    if let Some(default) = &column.default {
                        tip.push_str(&format!("\nDEFAULT {default}"));
                    }
                    if let Some(comment) = &column.comment {
                        tip.push_str(&format!("\n\n{comment}"));
                    }
                    ui.label(tip);
                });
                response.context_menu(|ui| {
                    if menu_item(ui, Icon::Copy, "Copy name") {
                        actions.push(Action::Copy(column.name.clone()));
                    }
                });
            }
        }
    }
}

/// Keys listed per page in the tree; more come on demand.
fn keyspace_node(
    ui: &mut Ui,
    id: ConnectionId,
    schema: &str,
    explorer: &mut Explorer,
    actions: &mut Vec<Action>,
) {
    let palette = Palette::current(ui.ctx());
    let key = NodeKey::Schema(id, schema.to_owned());
    let expanded = explorer.is_expanded(&key);
    let entry = (id, schema.to_owned());
    let keys = explorer.keys.get(&entry).cloned().unwrap_or_default();
    let own = matches!(explorer.link(id), Link::Online(server) if server.database == schema);
    let detail = keys.listed.then(|| {
        format!(
            "{}{}",
            keys.list.len(),
            if keys.next.is_some() { "+" } else { "" }
        )
    });
    let mut row = Row::new(1, Icon::Database, schema);
    row.expanded = Some(expanded);
    row.selected = explorer.selected.as_ref() == Some(&key);
    row.loading = keys.loading && !keys.listed;
    row.strong = own;
    row.detail = detail.as_deref();
    let response = row.show(ui);
    let response = if own {
        response.on_hover_text("The editor's database")
    } else {
        response
    };
    if response.clicked() {
        explorer.pick(key.clone());
        if explorer.toggle(key.clone()) && !keys.listed && !keys.loading {
            actions.push(Action::LoadKeys(id, schema.to_owned(), false));
        }
    }
    response.context_menu(|ui| {
        if menu_item(ui, Icon::Terminal, "New Redis editor") {
            let select = violet_lang::redis::actions::select(schema).unwrap_or_default();
            actions.push(Action::OpenEditor {
                connection: Some(id),
                sql: format!(
                    "{select}

"
                ),
            });
        }
        if menu_item(ui, Icon::Refresh, "Refresh") {
            actions.push(Action::LoadKeys(id, schema.to_owned(), false));
        }
        if menu_item(ui, Icon::Copy, "Copy name") {
            actions.push(Action::Copy(schema.to_owned()));
        }
    });
    if !expanded {
        return;
    }

    // The glob the server matches keys with, such as `user:*`.
    ui.horizontal(|ui| {
        ui.add_space(6.0 + 2.0 * INDENT + 16.0);
        let width = ui.available_width() - 14.0;
        let entry = explorer.keys.entry((id, schema.to_owned())).or_default();
        let applied = entry.applied.clone();
        egui::Frame::new()
            .fill(palette.surface)
            .stroke(egui::Stroke::new(1.0, palette.outline))
            .corner_radius(CornerRadius::same(6))
            .inner_margin(egui::Margin::symmetric(6, 0))
            .show(ui, |ui| {
                ui.set_width(width - 12.0);
                ui.horizontal(|ui| {
                    let (rect, _) = ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
                    let active = !applied.is_empty() && applied != "*";
                    widgets::paint_icon(
                        ui,
                        Icon::Funnel,
                        rect,
                        if active { palette.accent } else { palette.dim },
                    );
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut entry.pattern)
                            .id_salt(("key-pattern", id.0, schema))
                            .hint_text("Match keys, e.g. user:*")
                            .font(theme::mono(12.0))
                            .frame(egui::Frame::NONE)
                            .margin(egui::vec2(2.0, 4.0))
                            .desired_width(f32::INFINITY),
                    );
                    if response.lost_focus()
                        && ui.input(|input| input.key_pressed(egui::Key::Enter))
                    {
                        actions.push(Action::LoadKeys(id, schema.to_owned(), false));
                    }
                    if entry.glob() != applied && !applied.is_empty() {
                        response.on_hover_text("Press Enter to list the keys matching it");
                    }
                });
            });
    });

    if let Some(error) = &keys.error {
        error_row(ui, 2, error);
    }
    if !keys.listed {
        if keys.loading {
            loading_row(ui, 2);
        }
        return;
    }
    let filter = explorer.filter.trim().to_lowercase();
    let mut shown = 0;
    for object in keys.list.iter() {
        if !filter.is_empty() && !matches_filter(&object.name, &filter) {
            continue;
        }
        shown += 1;
        key_node(ui, id, object, explorer, actions);
    }
    if shown == 0 && !keys.loading {
        let mut row = Row::new(2, Icon::Info, "No keys match");
        row.icon_tint = Some(palette.dim);
        row.show(ui);
    }
    if keys.next.is_some() || keys.loading {
        let mut row = Row::new(2, Icon::ChevronsDown, "Load more keys…");
        row.icon_tint = Some(palette.accent);
        row.loading = keys.loading;
        if row.show(ui).clicked() && !keys.loading {
            actions.push(Action::LoadKeys(id, schema.to_owned(), true));
        }
    }
}

/// A key: its type's icon, its size, and on hover its type, size and TTL.
fn key_node(
    ui: &mut Ui,
    id: ConnectionId,
    object: &DbObject,
    explorer: &mut Explorer,
    actions: &mut Vec<Action>,
) {
    let key = NodeKey::Object(id, object.schema.clone(), object.name.clone());
    let detail = object.estimated_rows.map(compact_count);
    let mut row = Row::new(
        2,
        Icon::for_subtype(object.subtype.as_deref()),
        &object.name,
    );
    row.selected = explorer.selected.as_ref() == Some(&key);
    row.detail = detail.as_deref();
    let mut response = row.show(ui);
    if let Some(detail) = &object.detail {
        response = response.on_hover_text(detail);
    }
    let open = || Action::OpenData {
        connection: id,
        schema: object.schema.clone(),
        relation: object.name.clone(),
        subtype: object.subtype.clone(),
    };
    if response.clicked() {
        explorer.pick(key);
        actions.push(open());
    }
    let dialog = |op| Action::KeyDialog {
        connection: id,
        schema: object.schema.clone(),
        key: object.name.clone(),
        op,
    };
    response.context_menu(|ui| {
        if menu_item(ui, Icon::Rows, "View data") {
            actions.push(open());
        }
        if menu_item(ui, Icon::Terminal, "New Redis editor") {
            let select = violet_lang::redis::actions::select(&object.schema).unwrap_or_default();
            let read = violet_lang::redis::browse(object.subtype.as_deref(), &object.name, None);
            actions.push(Action::OpenEditor {
                connection: Some(id),
                sql: format!(
                    "{select}
{read}
"
                ),
            });
        }
        if menu_item(ui, Icon::Copy, "Copy name") {
            actions.push(Action::Copy(object.name.clone()));
        }
        ui.separator();
        if menu_item(ui, Icon::Clock, "Set TTL…") {
            actions.push(dialog(KeyOp::Ttl));
        }
        if menu_item(ui, Icon::Pencil, "Rename…") {
            actions.push(dialog(KeyOp::Rename));
        }
        if menu_item(ui, Icon::Trash, "Delete…") {
            actions.push(dialog(KeyOp::Delete));
        }
    });
}

fn loading_row(ui: &mut Ui, depth: usize) {
    let mut row = Row::new(depth, Icon::Loader, "Loading…");
    row.loading = true;
    row.show(ui);
}

fn error_row(ui: &mut Ui, depth: usize, error: &str) {
    let palette = Palette::current(ui.ctx());
    let mut row = Row::new(depth, Icon::CircleAlert, error);
    row.icon_tint = Some(palette.danger);
    row.show(ui).on_hover_text(error);
}

/// A menu entry with an icon. Returns whether it was chosen.
///
/// Menus lay entries out justified, so each one spans the widest entry; the
/// menu is as wide as its longest label, not as wide as the window.
pub fn menu_item(ui: &mut Ui, icon: Icon, label: &str) -> bool {
    let palette = Palette::current(ui.ctx());
    let image = icon.image(palette.secondary, 14.0);
    ui.add(
        egui::Button::image_and_text(image, label)
            .frame(false)
            .min_size(Vec2::new(MENU_MIN_WIDTH, 26.0)),
    )
    .clicked()
}

/// Narrowest a context menu gets, so short menus do not look cramped.
const MENU_MIN_WIDTH: f32 = 150.0;

/// Whether `name` contains `filter` (already lower case), ignoring case.
/// ASCII filters, the usual kind, compare in place without allocating.
fn matches_filter(name: &str, filter: &str) -> bool {
    if filter.is_ascii() {
        let needle = filter.as_bytes();
        needle.is_empty()
            || name
                .as_bytes()
                .windows(needle.len())
                .any(|window| window.eq_ignore_ascii_case(needle))
    } else {
        name.to_lowercase().contains(filter)
    }
}

fn quote(ident: &str) -> String {
    let plain = ident
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && !ident.starts_with(|c: char| c.is_ascii_digit());
    if plain && !ident.is_empty() {
        ident.to_owned()
    } else {
        format!("\"{}\"", ident.replace('"', "\"\""))
    }
}

/// `1234567` as `1.2M`.
fn compact_count(count: i64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let value = count as f64;
    if count >= 1_000_000_000 {
        format!("{:.1}B", value / 1e9)
    } else if count >= 1_000_000 {
        format!("{:.1}M", value / 1e6)
    } else if count >= 1_000 {
        format!("{:.1}k", value / 1e3)
    } else {
        count.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_are_quoted_only_when_needed() {
        assert_eq!(quote("users"), "users");
        assert_eq!(quote("Users"), "\"Users\"");
        assert_eq!(quote("my table"), "\"my table\"");
        assert_eq!(quote("1st"), "\"1st\"");
        assert_eq!(quote("a\"b"), "\"a\"\"b\"");
    }

    #[test]
    fn filters_ignore_case() {
        assert!(matches_filter("UserAccounts", "account"));
        assert!(matches_filter("anything", ""));
        assert!(!matches_filter("orders", "user"));
        assert!(matches_filter("Ação", "açã"));
    }

    #[test]
    fn counts_are_compact() {
        assert_eq!(compact_count(999), "999");
        assert_eq!(compact_count(1_250), "1.2k");
        assert_eq!(compact_count(3_400_000), "3.4M");
    }
}
