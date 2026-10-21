//! The window's frame: title bar, tab strip, editor toolbar, status bar and
//! the welcome page.

use egui::{Align, Align2, CornerRadius, Layout, RichText, Sense, Stroke, Ui, Vec2};
use violet_core::{ConnectionId, ConnectionProfile};

use super::action::{Action, RunScope};
use super::icons::Icon;
use super::shortcuts::{self, Command};
use super::sidebar::menu_item;
use super::state::{Explorer, Link};
use super::theme::{self, Palette};
use super::widgets::{self, Tone};
use super::workspace::{DataView, QueryTab, TabId, Workspace, format_duration, thousands};

pub const TITLE_BAR_HEIGHT: f32 = 44.0;

/// The title bar: brand, breadcrumb and global actions. On macOS it is
/// drawn under the traffic lights and drags the window.
pub fn title_bar(
    ui: &mut Ui,
    title: &str,
    theme_preference: egui::ThemePreference,
    update: &crate::update::Status,
    actions: &mut Vec<Action>,
) {
    let palette = Palette::current(ui.ctx());
    let rect = ui.max_rect();
    let background = ui.interact(rect, ui.id().with("title-drag"), Sense::click_and_drag());
    if cfg!(target_os = "macos") {
        if background.drag_started() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
        if background.double_clicked() {
            match fastframe_macos::double_click_action() {
                fastframe_macos::DoubleClick::Minimize => {
                    ui.ctx()
                        .send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                }
                fastframe_macos::DoubleClick::Nothing => {}
                fastframe_macos::DoubleClick::Zoom | fastframe_macos::DoubleClick::Fill => {
                    let maximized = ui.input(|input| input.viewport().maximized.unwrap_or(false));
                    ui.ctx()
                        .send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
                }
            }
        }
    }

    ui.horizontal_centered(|ui| {
        ui.add_space(fastframe_macos::traffic_light_inset(ui.ctx()) + 12.0);
        if widgets::icon_button(ui, Icon::PanelLeft, "Toggle sidebar").clicked() {
            actions.push(Action::ToggleSidebar);
        }
        ui.add_space(4.0);
        let (logo, _) = ui.allocate_exact_size(Vec2::splat(22.0), Sense::hover());
        ui.painter()
            .rect_filled(logo, CornerRadius::same(6), palette.accent);
        widgets::paint_icon(ui, Icon::Logo, logo.shrink(4.0), palette.on_accent);
        ui.label(
            RichText::new("Violet")
                .font(theme::bold(14.5))
                .color(palette.text),
        );
        if !title.is_empty() {
            ui.label(RichText::new("/").color(palette.dim));
            ui.label(
                RichText::new(title)
                    .font(theme::medium(13.0))
                    .color(palette.secondary),
            );
        }

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.add_space(10.0);
            if widgets::button(ui, Tone::Primary, Some(Icon::Plus), "Connection").clicked() {
                actions.push(Action::NewConnection);
            }
            if widgets::icon_button(ui, Icon::Terminal, "New SQL editor").clicked() {
                actions.push(Action::OpenEditor {
                    connection: None,
                    sql: String::new(),
                });
            }
            if widgets::icon_button(ui, Icon::Package, "Database providers").clicked() {
                actions.push(Action::OpenProviders);
            }
            if widgets::icon_button(ui, Icon::Keyboard, "Keyboard shortcuts").clicked() {
                actions.push(Action::OpenShortcuts);
            }
            let (icon, next, tip) = match theme_preference {
                egui::ThemePreference::System => {
                    (Icon::Monitor, egui::ThemePreference::Light, "Theme: system")
                }
                egui::ThemePreference::Light => {
                    (Icon::Sun, egui::ThemePreference::Dark, "Theme: light")
                }
                egui::ThemePreference::Dark => {
                    (Icon::Moon, egui::ThemePreference::System, "Theme: dark")
                }
            };
            if widgets::icon_button(ui, icon, tip).clicked() {
                actions.push(Action::SetTheme(next));
            }
            update_badge(ui, update, actions);
        });
    });
}

/// A new version: downloading, ready to install on restart, or to be
/// downloaded by hand.
fn update_badge(ui: &mut Ui, update: &crate::update::Status, actions: &mut Vec<Action>) {
    use crate::update::Status;
    let palette = Palette::current(ui.ctx());
    match update {
        Status::Idle => {}
        Status::Downloading { version, percent } => {
            ui.label(
                RichText::new(format!("Downloading Violet {version}… {percent}%"))
                    .font(theme::medium(12.0))
                    .color(palette.secondary),
            );
        }
        Status::Ready { version } => {
            if widgets::button(
                ui,
                Tone::Secondary,
                Some(Icon::Download),
                "Restart to update",
            )
            .on_hover_text(format!("Violet {version} is downloaded and verified"))
            .clicked()
            {
                actions.push(Action::RestartToUpdate);
            }
        }
        Status::Installing { version } => {
            ui.label(
                RichText::new(format!("Installing Violet {version}…"))
                    .font(theme::medium(12.0))
                    .color(palette.secondary),
            );
        }
        Status::Manual {
            version,
            url,
            reason,
        } => {
            // Labelled, not a bare icon: this copy cannot update itself and
            // the reason has to be seen.
            if widgets::button(
                ui,
                Tone::Secondary,
                Some(Icon::Download),
                &format!("Violet {version} available"),
            )
            .on_hover_text(format!("{reason}\n\nClick to open the release page."))
            .clicked()
            {
                actions.push(Action::OpenUrl(url.clone()));
            }
        }
        Status::Failed {
            version,
            url,
            error,
        } => {
            let response = widgets::button(
                ui,
                Tone::Secondary,
                Some(Icon::CircleAlert),
                "Update failed",
            )
            .on_hover_text(format!(
                "Violet {version} could not be installed: {error}\n\n\
                 Click to try again, or right-click to open the release page."
            ));
            if response.clicked() {
                actions.push(Action::RetryUpdate);
            }
            if response.secondary_clicked() {
                actions.push(Action::OpenUrl(url.clone()));
            }
        }
    }
}

/// Tabs along the top of the workspace.
pub fn tab_strip(
    ui: &mut Ui,
    workspace: &Workspace,
    profiles: &[ConnectionProfile],
    actions: &mut Vec<Action>,
) {
    let palette = Palette::current(ui.ctx());
    let bar = ui.max_rect();
    ui.painter().rect_filled(bar, 0.0, palette.panel);
    ui.painter().hline(
        bar.x_range(),
        bar.bottom() - 0.5,
        Stroke::new(1.0, palette.outline),
    );

    egui::ScrollArea::horizontal()
        .id_salt("tab-strip")
        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                for (index, tab) in workspace.tabs.iter().enumerate() {
                    let active = workspace.active == Some(tab.id);
                    let color = tab
                        .connection
                        .and_then(|id| profiles.iter().find(|p| p.id == id))
                        .and_then(|p| p.color)
                        .map(theme::tag);
                    tab_button(ui, tab, active, color, actions)
                        .context_menu(|ui| tab_menu(ui, workspace, index, actions));
                }
                ui.add_space(4.0);
                if widgets::icon_button_sized(ui, Icon::Plus, "New SQL editor", 26.0, 15.0)
                    .clicked()
                {
                    let connection = workspace.active().and_then(|tab| tab.connection);
                    actions.push(Action::OpenEditor {
                        connection,
                        sql: String::new(),
                    });
                }
            });
        });
}

/// A tab's context menu: closing it or several at once, as editors do,
/// and its script's file.
fn tab_menu(ui: &mut Ui, workspace: &Workspace, index: usize, actions: &mut Vec<Action>) {
    let ids: Vec<TabId> = workspace.tabs.iter().map(|tab| tab.id).collect();
    let tab = &workspace.tabs[index];
    let mut close = |ui: &mut Ui, label: &str, icon: Icon, ids: &[TabId]| {
        let clicked = ui
            .add_enabled_ui(!ids.is_empty(), |ui| menu_item(ui, icon, label))
            .inner;
        if clicked {
            actions.push(Action::CloseTabs(ids.to_vec()));
        }
    };
    close(ui, "Close", Icon::Close, &[tab.id]);
    let others: Vec<TabId> = ids.iter().copied().filter(|id| *id != tab.id).collect();
    close(ui, "Close others", Icon::Close, &others);
    close(ui, "Close to the left", Icon::Close, &ids[..index]);
    close(ui, "Close to the right", Icon::Close, &ids[index + 1..]);
    close(ui, "Close all", Icon::Close, &ids);
    if let Some(file) = &tab.file {
        ui.separator();
        if menu_item(ui, Icon::Copy, "Copy path") {
            actions.push(Action::Copy(file.path.display().to_string()));
        }
        if menu_item(ui, Icon::FolderOpen, "Show in folder") {
            actions.push(Action::Reveal(file.path.clone()));
        }
    }
}

fn tab_button(
    ui: &mut Ui,
    tab: &QueryTab,
    active: bool,
    color: Option<egui::Color32>,
    actions: &mut Vec<Action>,
) -> egui::Response {
    let palette = Palette::current(ui.ctx());
    let height = ui.available_height().max(36.0);
    // A dot after the title: changes the script's file does not have.
    let title = if tab.unsaved() {
        format!("{} ●", tab.title)
    } else {
        tab.title.clone()
    };
    let title_galley = widgets::elided(ui, &title, theme::medium(12.5), palette.text, 170.0);
    let width = 14.0 + 16.0 + 8.0 + title_galley.size().x + 8.0 + 18.0 + 10.0;
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, height), Sense::click());
    let painter = ui.painter();

    if active {
        painter.rect_filled(rect, 0.0, palette.window);
        painter.hline(
            rect.x_range(),
            rect.top() + 1.0,
            Stroke::new(2.0, palette.accent),
        );
    } else if response.hovered() {
        painter.rect_filled(rect, 0.0, palette.surface_hover);
    }
    painter.vline(
        rect.right() - 0.5,
        rect.y_range(),
        Stroke::new(1.0, palette.outline),
    );

    let center = rect.center().y;
    let mut x = rect.left() + 14.0;
    let icon = if tab.data().is_some() {
        Icon::Table
    } else {
        Icon::FileCode
    };
    let tint = color.unwrap_or(if active { palette.accent } else { palette.dim });
    widgets::paint_icon(
        ui,
        icon,
        egui::Rect::from_center_size(egui::pos2(x + 7.0, center), Vec2::splat(14.0)),
        tint,
    );
    x += 22.0;
    let text_color = if active {
        palette.text
    } else {
        palette.secondary
    };
    painter.galley(
        egui::pos2(x, center - title_galley.size().y / 2.0),
        title_galley.clone(),
        text_color,
    );
    x += title_galley.size().x + 8.0;

    let close_rect = egui::Rect::from_center_size(egui::pos2(x + 9.0, center), Vec2::splat(18.0));
    let close = ui.interact(
        close_rect,
        ui.id().with(("close-tab", tab.id)),
        Sense::click(),
    );
    if tab.running.is_some() {
        ui.put(
            close_rect,
            egui::Spinner::new().size(12.0).color(palette.accent),
        );
    } else if active || response.hovered() || close.hovered() {
        if close.hovered() {
            ui.painter()
                .rect_filled(close_rect, CornerRadius::same(4), palette.surface_active);
        }
        widgets::paint_icon(ui, Icon::Close, close_rect.shrink(3.0), palette.secondary);
    }
    if close.clicked() || response.middle_clicked() {
        actions.push(Action::CloseTab(tab.id));
    } else if response.clicked() {
        actions.push(Action::SelectTab(tab.id));
    }
    response
}

/// The bar above an editor: connection, run buttons.
pub fn toolbar(
    ui: &mut Ui,
    tab: &QueryTab,
    profiles: &[ConnectionProfile],
    explorer: &Explorer,
    actions: &mut Vec<Action>,
) {
    let palette = Palette::current(ui.ctx());
    if let Some(view) = tab.data() {
        data_toolbar(ui, tab, view, profiles, actions);
        return;
    }
    ui.horizontal_centered(|ui| {
        ui.add_space(10.0);
        let running = tab.running.is_some();
        let run_tip = format!(
            "Run statement ({})",
            ui.ctx()
                .format_shortcut(&shortcuts::get(Command::RunStatement))
        );
        let response = ui
            .add_enabled_ui(!running, |ui| {
                widgets::button(ui, Tone::Primary, Some(Icon::Play), "Run")
            })
            .inner;
        if response.on_hover_text(run_tip).clicked() {
            actions.push(Action::Run(tab.id, RunScope::Statement));
        }
        let script_tip = format!(
            "Run script ({})",
            ui.ctx()
                .format_shortcut(&shortcuts::get(Command::RunScript))
        );
        let response = ui
            .add_enabled_ui(!running, |ui| {
                widgets::button(ui, Tone::Secondary, Some(Icon::FileCode), "Run script")
            })
            .inner;
        if response.on_hover_text(script_tip).clicked() {
            actions.push(Action::Run(tab.id, RunScope::Script));
        }
        let explain = ui
            .add_enabled_ui(!running, |ui| {
                widgets::button(ui, Tone::Secondary, Some(Icon::Zap), "Explain")
            })
            .inner
            .on_hover_text("How the server runs the statement under the cursor");
        egui::Popup::menu(&explain).show(|ui| {
            let ctx = ui.ctx().clone();
            for (icon, label, shortcut, scope) in [
                (
                    Icon::Zap,
                    "Explain plan",
                    shortcuts::get(Command::Explain),
                    RunScope::Explain,
                ),
                (
                    Icon::Clock,
                    "Explain analyze (reads only)",
                    shortcuts::get(Command::ExplainAnalyze),
                    RunScope::ExplainAnalyze,
                ),
            ] {
                if mode_item(ui, icon, label, &ctx.format_shortcut(&shortcut)) {
                    actions.push(Action::Run(tab.id, scope));
                }
            }
        });
        let cancel_tip = format!(
            "Cancel ({})",
            ui.ctx().format_shortcut(&shortcuts::get(Command::Cancel))
        );
        if ui
            .add_enabled_ui(running, |ui| {
                widgets::icon_button(ui, Icon::Stop, &cancel_tip)
            })
            .inner
            .clicked()
        {
            actions.push(Action::Cancel(tab.id));
        }
        let format_tip = format!(
            "Format the selection, or the whole script ({})",
            ui.ctx().format_shortcut(&shortcuts::get(Command::Format))
        );
        if widgets::icon_button(ui, Icon::Function, &format_tip).clicked() {
            actions.push(Action::FormatSql(tab.id));
        }
        let find_tip = format!(
            "Find ({}), replace ({})",
            ui.ctx().format_shortcut(&shortcuts::get(Command::Find)),
            ui.ctx().format_shortcut(&shortcuts::get(Command::Replace))
        );
        if widgets::icon_button(ui, Icon::Search, &find_tip).clicked() {
            actions.push(Action::Find(tab.id, false));
        }
        let save_tip = match &tab.file {
            Some(file) => format!(
                "Save ({})\n{}",
                ui.ctx().format_shortcut(&shortcuts::get(Command::Save)),
                file.path.display()
            ),
            None => format!(
                "Save script ({})",
                ui.ctx().format_shortcut(&shortcuts::get(Command::Save))
            ),
        };
        let save = widgets::icon_button(ui, Icon::Save, &save_tip);
        if save.clicked() {
            actions.push(Action::SaveScript(tab.id));
        }
        save.context_menu(|ui| {
            let label = format!(
                "Save as… ({})",
                ui.ctx().format_shortcut(&shortcuts::get(Command::SaveAs))
            );
            if ui.button(label).clicked() {
                actions.push(Action::SaveScriptAs(tab.id));
            }
            let label = format!(
                "Open… ({})",
                ui.ctx().format_shortcut(&shortcuts::get(Command::Open))
            );
            if ui.button(label).clicked() {
                actions.push(Action::OpenScriptFile);
            }
        });

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.add_space(10.0);
            connection_picker(ui, tab, profiles, explorer, actions);
            ui.label(RichText::new("Connection").color(palette.dim));
            let history_tip = format!(
                "Query history ({})",
                ui.ctx().format_shortcut(&shortcuts::get(Command::History))
            );
            if widgets::icon_button(ui, Icon::Clock, &history_tip).clicked() {
                actions.push(Action::OpenHistory);
            }
        });
    });
}

/// A menu entry with its shortcut on the right.
fn mode_item(ui: &mut Ui, icon: Icon, label: &str, shortcut: &str) -> bool {
    let palette = Palette::current(ui.ctx());
    ui.add(
        egui::Button::image_and_text(icon.image(palette.secondary, 14.0), label)
            .shortcut_text(RichText::new(shortcut).color(palette.dim))
            .frame(false)
            .min_size(Vec2::new(250.0, 26.0)),
    )
    .clicked()
}

/// A data tab's bar: refresh, stop, and what is being browsed.
fn data_toolbar(
    ui: &mut Ui,
    tab: &QueryTab,
    view: &DataView,
    profiles: &[ConnectionProfile],
    actions: &mut Vec<Action>,
) {
    let palette = Palette::current(ui.ctx());
    ui.horizontal_centered(|ui| {
        ui.add_space(10.0);
        let running = tab.running.is_some();
        let refresh = ui
            .add_enabled_ui(!running, |ui| {
                widgets::button(ui, Tone::Primary, Some(Icon::Refresh), "Refresh")
            })
            .inner;
        if refresh
            .on_hover_text("Read the rows again from the server, past the cache")
            .clicked()
        {
            actions.push(Action::RefreshTab(tab.id));
        }
        let cancel_tip = format!(
            "Cancel ({})",
            ui.ctx().format_shortcut(&shortcuts::get(Command::Cancel))
        );
        if ui
            .add_enabled_ui(running, |ui| {
                widgets::icon_button(ui, Icon::Stop, &cancel_tip)
            })
            .inner
            .clicked()
        {
            actions.push(Action::Cancel(tab.id));
        }
        if widgets::icon_button(ui, Icon::Terminal, "Open this query in a SQL editor").clicked() {
            actions.push(Action::OpenEditor {
                connection: tab.connection,
                sql: format!("{};\n", tab.sql),
            });
        }

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.add_space(12.0);
            let connection = tab
                .connection
                .and_then(|id| profiles.iter().find(|p| p.id == id))
                .map(ConnectionProfile::display_name)
                .unwrap_or_default();
            ui.label(RichText::new(connection).color(palette.dim));
            ui.label(
                RichText::new(format!("{}.{}", view.schema, view.relation))
                    .font(theme::mono(12.5))
                    .color(palette.secondary),
            );
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(14.0), Sense::hover());
            widgets::paint_icon(ui, Icon::Table, rect, palette.accent);
        });
    });
}

/// DBeaver's filter box: a `WHERE` condition applied on Enter.
pub fn filter_bar(ui: &mut Ui, tab: &mut QueryTab, actions: &mut Vec<Action>) {
    let palette = Palette::current(ui.ctx());
    let id = tab.id;
    let Some(view) = tab.data_mut() else {
        return;
    };
    ui.horizontal_centered(|ui| {
        ui.add_space(12.0);
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(14.0), Sense::hover());
        let active = !view.applied.is_empty();
        widgets::paint_icon(
            ui,
            Icon::Funnel,
            rect,
            if active { palette.accent } else { palette.dim },
        );
        ui.label(
            RichText::new("WHERE")
                .font(theme::mono(12.0))
                .color(palette.syntax.keyword),
        );
        let changed = view.filter.trim() != view.applied;
        let width = (ui.available_width() - 120.0).max(120.0);
        let response = ui.add(
            egui::TextEdit::singleline(&mut view.filter)
                .id_salt(("filter", id))
                .hint_text("Filter rows, e.g. id > 10 AND name ILIKE 'a%'  —  press Enter")
                .font(theme::mono(12.5))
                .frame(egui::Frame::NONE)
                .desired_width(width),
        );
        if response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter)) {
            actions.push(Action::ApplyFilter(id));
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.add_space(10.0);
            if (active || !view.filter.is_empty())
                && widgets::icon_button(ui, Icon::Close, "Clear the filter").clicked()
            {
                actions.push(Action::ClearFilter(id));
            }
            if changed
                && widgets::icon_button(ui, Icon::Check, "Apply the filter (Enter)").clicked()
            {
                actions.push(Action::ApplyFilter(id));
            }
        });
    });
}

fn connection_picker(
    ui: &mut Ui,
    tab: &QueryTab,
    profiles: &[ConnectionProfile],
    explorer: &Explorer,
    actions: &mut Vec<Action>,
) {
    let palette = Palette::current(ui.ctx());
    let selected = tab
        .connection
        .and_then(|id| profiles.iter().find(|profile| profile.id == id))
        .map_or_else(|| "Choose…".to_owned(), ConnectionProfile::display_name);
    egui::ComboBox::from_id_salt(("tab-connection", tab.id))
        .selected_text(RichText::new(selected).font(theme::medium(12.5)))
        .width(200.0)
        .show_ui(ui, |ui| {
            for profile in profiles {
                let online = explorer.link(profile.id).is_online();
                let dot = if online { "●  " } else { "○  " };
                let text =
                    RichText::new(format!("{dot}{}", profile.display_name())).color(if online {
                        palette.text
                    } else {
                        palette.secondary
                    });
                if ui
                    .selectable_label(tab.connection == Some(profile.id), text)
                    .clicked()
                {
                    actions.push(Action::SetTabConnection(tab.id, Some(profile.id)));
                }
            }
            if profiles.is_empty() {
                ui.label(RichText::new("No connections").color(palette.dim));
            }
        });
}

/// The strip along the bottom of the window.
pub fn status_bar(
    ui: &mut Ui,
    tab: Option<&QueryTab>,
    profiles: &[ConnectionProfile],
    explorer: &Explorer,
) {
    let palette = Palette::current(ui.ctx());
    ui.horizontal_centered(|ui| {
        ui.add_space(12.0);
        ui.spacing_mut().item_spacing.x = 6.0;
        let connection: Option<(ConnectionId, &ConnectionProfile)> = tab
            .and_then(|tab| tab.connection)
            .and_then(|id| profiles.iter().find(|p| p.id == id).map(|p| (id, p)));
        let small = |text: String, color| RichText::new(text).size(11.5).color(color);
        match connection {
            Some((id, profile)) => {
                let (color, state) = match explorer.link(id) {
                    Link::Online(server) => (
                        palette.success,
                        format!("{} · {}", server.version, server.database),
                    ),
                    Link::Connecting => (palette.warning, "Connecting…".to_owned()),
                    Link::Failed(_) => (palette.danger, "Connection failed".to_owned()),
                    Link::Offline => (palette.dim, "Disconnected".to_owned()),
                };
                widgets::dot(ui, color, 3.5);
                ui.label(small(profile.display_name(), palette.secondary));
                ui.label(small(state, palette.dim));
            }
            None => {
                ui.label(small("No connection selected".to_owned(), palette.dim));
            }
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.add_space(12.0);
            ui.label(small(
                format!("Violet {}", env!("CARGO_PKG_VERSION")),
                palette.dim,
            ));
            if let Some(super::workspace::Outcome::Done { output, .. }) =
                tab.and_then(|tab| tab.outcome.as_ref())
            {
                let rows: usize = output.sets.iter().map(|set| set.rows.len()).sum();
                ui.label(small(
                    format!(
                        "{} rows · {}",
                        thousands(rows),
                        format_duration(output.elapsed)
                    ),
                    palette.secondary,
                ));
            }
        });
    });
}

/// Shown when no tab is open.
pub fn welcome(
    ui: &mut Ui,
    profiles: &[ConnectionProfile],
    explorer: &Explorer,
    actions: &mut Vec<Action>,
) {
    let palette = Palette::current(ui.ctx());
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space((ui.available_height() * 0.14).max(32.0));
                let (logo, _) = ui.allocate_exact_size(Vec2::splat(64.0), Sense::hover());
                ui.painter()
                    .rect_filled(logo, CornerRadius::same(18), palette.accent);
                widgets::paint_icon(ui, Icon::Logo, logo.shrink(14.0), palette.on_accent);
                ui.add_space(14.0);
                ui.label(
                    RichText::new("Violet")
                        .font(theme::bold(30.0))
                        .color(palette.text),
                );
                ui.label(
                    RichText::new("A fast, modern client for every database.")
                        .size(15.0)
                        .color(palette.secondary),
                );
                ui.add_space(22.0);

                let width = 2.0 * 160.0 + 10.0;
                ui.allocate_ui_with_layout(
                    Vec2::new(width, 32.0),
                    Layout::left_to_right(Align::Center),
                    |ui| {
                        if widgets::button(ui, Tone::Primary, Some(Icon::Plus), "New connection")
                            .clicked()
                        {
                            actions.push(Action::NewConnection);
                        }
                        if widgets::button(
                            ui,
                            Tone::Secondary,
                            Some(Icon::Terminal),
                            "Open SQL editor",
                        )
                        .clicked()
                        {
                            actions.push(Action::OpenEditor {
                                connection: profiles.first().map(|p| p.id),
                                sql: String::new(),
                            });
                        }
                    },
                );

                if !profiles.is_empty() {
                    ui.add_space(36.0);
                    ui.allocate_ui_with_layout(
                        Vec2::new(560.0, 0.0),
                        Layout::top_down(Align::Min),
                        |ui| {
                            widgets::caption(ui, "Connections");
                            ui.add_space(6.0);
                            for profile in profiles.iter().take(6) {
                                connection_card(ui, profile, explorer, actions);
                            }
                        },
                    );
                }

                ui.add_space(36.0);
                ui.allocate_ui_with_layout(
                    Vec2::new(560.0, 0.0),
                    Layout::top_down(Align::Min),
                    |ui| {
                        widgets::caption(ui, "Shortcuts");
                        ui.add_space(6.0);
                        let ctx = ui.ctx().clone();
                        egui::Grid::new("shortcuts")
                            .num_columns(2)
                            .spacing([40.0, 8.0])
                            .show(ui, |ui| {
                                for (label, shortcut) in [
                                    (
                                        "Run statement under cursor",
                                        shortcuts::get(Command::RunStatement),
                                    ),
                                    ("Run whole script", shortcuts::get(Command::RunScript)),
                                    ("Cancel running query", shortcuts::get(Command::Cancel)),
                                    ("Explain the statement", shortcuts::get(Command::Explain)),
                                    ("Query history", shortcuts::get(Command::History)),
                                    ("New SQL editor", shortcuts::get(Command::NewEditor)),
                                    ("New connection", shortcuts::get(Command::NewConnection)),
                                    ("Close tab", shortcuts::get(Command::CloseTab)),
                                    ("Toggle sidebar", shortcuts::get(Command::ToggleSidebar)),
                                ] {
                                    ui.label(RichText::new(label).color(palette.secondary));
                                    widgets::pill(
                                        ui,
                                        &ctx.format_shortcut(&shortcut),
                                        palette.text,
                                        palette.surface_hover,
                                    );
                                    ui.end_row();
                                }
                            });
                    },
                );
                ui.add_space(32.0);
            });
        });
}

fn connection_card(
    ui: &mut Ui,
    profile: &ConnectionProfile,
    explorer: &Explorer,
    actions: &mut Vec<Action>,
) {
    let palette = Palette::current(ui.ctx());
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 52.0), Sense::click());
    let hovered = response.hovered();
    ui.painter().rect(
        rect,
        CornerRadius::same(10),
        if hovered {
            palette.surface_hover
        } else {
            palette.surface
        },
        Stroke::new(
            1.0,
            if hovered {
                palette.accent.gamma_multiply(0.6)
            } else {
                palette.outline
            },
        ),
        egui::StrokeKind::Inside,
    );
    let tag = profile.color.map_or(palette.accent, theme::tag);
    let icon_rect = egui::Rect::from_center_size(
        egui::pos2(rect.left() + 28.0, rect.center().y),
        Vec2::splat(32.0),
    );
    ui.painter()
        .rect_filled(icon_rect, CornerRadius::same(8), tag.gamma_multiply(0.16));
    widgets::paint_icon(ui, Icon::Database, icon_rect.shrink(8.0), tag);

    let text_left = rect.left() + 54.0;
    let name_rect = egui::Rect::from_min_max(
        egui::pos2(text_left, rect.top() + 8.0),
        egui::pos2(rect.right() - 120.0, rect.center().y + 1.0),
    );
    widgets::paint_text(
        ui,
        name_rect,
        Align2::LEFT_CENTER,
        &profile.display_name(),
        theme::semibold(13.5),
        palette.text,
    );
    let detail = if profile.is_file() {
        profile.host.clone()
    } else {
        format!(
            "{}@{}:{}/{}",
            profile.username, profile.host, profile.port, profile.database
        )
    };
    let detail_rect = egui::Rect::from_min_max(
        egui::pos2(text_left, rect.center().y),
        egui::pos2(rect.right() - 120.0, rect.bottom() - 8.0),
    );
    widgets::paint_text(
        ui,
        detail_rect,
        Align2::LEFT_CENTER,
        &detail,
        theme::mono(11.5),
        palette.dim,
    );

    let (label, color) = match explorer.link(profile.id) {
        Link::Online(_) => ("Connected", palette.success),
        Link::Connecting => ("Connecting…", palette.warning),
        Link::Failed(_) => ("Failed", palette.danger),
        Link::Offline => ("Open", palette.secondary),
    };
    let label_rect = egui::Rect::from_min_max(
        egui::pos2(rect.right() - 110.0, rect.top()),
        egui::pos2(rect.right() - 16.0, rect.bottom()),
    );
    widgets::paint_text(
        ui,
        label_rect,
        Align2::RIGHT_CENTER,
        label,
        theme::medium(12.5),
        color,
    );

    if response.clicked() {
        if !explorer.link(profile.id).is_online() {
            actions.push(Action::Connect(profile.id));
        }
        actions.push(Action::OpenEditor {
            connection: Some(profile.id),
            sql: String::new(),
        });
    }
    ui.add_space(8.0);
}
