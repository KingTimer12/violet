//! The dialog that creates and edits connections.

use egui::{CornerRadius, RichText, Sense, Stroke, Vec2};
use violet_app::RequestId;
use violet_core::uri::ParsedUri;
use violet_core::{ConnectionProfile, DriverInfo, Login, Secret, ServerInfo, SslMode, is_loopback};
use zeroize::Zeroize as _;

use super::action::Action;
use super::icons::Icon;
use super::providers::{DriverChoice, format_size};
use super::theme::{self, Palette};
use super::widgets::{self, Tone};

#[derive(Debug, Default)]
pub enum TestState {
    #[default]
    Idle,
    Running(RequestId),
    Passed(ServerInfo),
    Failed(String),
}

pub struct ConnectionForm {
    pub profile: ConnectionProfile,
    password: String,
    /// The profile already has a password in the keychain.
    has_saved_password: bool,
    pub editing: bool,
    show_password: bool,
    port: String,
    pub test: TestState,
    error: Option<String>,
    /// A connection string being pasted.
    uri: String,
}

impl Drop for ConnectionForm {
    fn drop(&mut self) {
        self.password.zeroize();
        self.uri.zeroize();
    }
}

impl ConnectionForm {
    pub fn create(driver: &DriverInfo) -> Self {
        let mut profile = ConnectionProfile::new(driver.id.clone(), driver.default_port);
        profile.database.clone_from(&driver.default_database);
        profile.username.clone_from(&driver.default_user);
        Self::with(profile, false, false)
    }

    /// Picks another driver, moving fields still at the old one's defaults
    /// to the new one's.
    fn switch_driver(&mut self, from: Option<&DriverInfo>, to: &DriverInfo) {
        self.profile.driver = to.id.clone();
        let untouched =
            |value: &str, default: Option<&str>| value.trim().is_empty() || Some(value) == default;
        if untouched(
            &self.port,
            from.map(|f| f.default_port.to_string()).as_deref(),
        ) {
            self.port = to.default_port.to_string();
        }
        if untouched(
            &self.profile.database,
            from.map(|f| f.default_database.as_str()),
        ) {
            self.profile.database.clone_from(&to.default_database);
        }
        if untouched(
            &self.profile.username,
            from.map(|f| f.default_user.as_str()),
        ) {
            self.profile.username.clone_from(&to.default_user);
        }
        // Between a server and a database file, the host means something
        // else: a host name, or the file's path.
        let was_file = from.is_some_and(|f| f.default_port == 0);
        let is_file = to.default_port == 0;
        if is_file && !was_file {
            self.port = "0".to_owned();
            self.profile.host.clear();
        } else if was_file && !is_file {
            self.profile.host = "localhost".to_owned();
        }
    }

    pub fn edit(profile: ConnectionProfile, has_saved_password: bool) -> Self {
        Self::with(profile, true, has_saved_password)
    }

    fn with(profile: ConnectionProfile, editing: bool, has_saved_password: bool) -> Self {
        Self {
            port: profile.port.to_string(),
            profile,
            password: String::new(),
            has_saved_password,
            editing,
            show_password: false,
            test: TestState::Idle,
            error: None,
            uri: String::new(),
        }
    }

    /// Fills the form from a connection string, for `driver`.
    pub fn apply_uri(&mut self, parsed: ParsedUri, driver: &DriverInfo) {
        self.profile.host = parsed.host;
        self.port = parsed.port.unwrap_or(driver.default_port).to_string();
        self.profile.database = if parsed.database.is_empty() {
            driver.default_database.clone()
        } else {
            parsed.database
        };
        self.profile.username = parsed.username;
        if let Some(ssl) = parsed.ssl {
            self.profile.ssl = ssl;
        }
        if let Some(mut password) = parsed.password {
            self.password.zeroize();
            std::mem::swap(&mut self.password, &mut password);
        }
        if self.profile.name.trim().is_empty() {
            self.profile.name = format!("{} · {}", driver.name, self.profile.host);
        }
        self.uri.zeroize();
        self.error = None;
    }

    /// The password typed, if any. Empty means "keep the saved one".
    fn typed_password(&self) -> Option<Secret> {
        (!self.password.is_empty()).then(|| Secret::new(self.password.clone()))
    }

    /// Checks the form and returns the profile it describes.
    fn finish(&mut self, user: Login) -> Option<ConnectionProfile> {
        let Ok(port) = self.port.trim().parse::<u16>() else {
            self.error = Some("Port must be a number between 1 and 65535".to_owned());
            return None;
        };
        let mut profile = self.profile.clone();
        profile.port = port;
        profile.name = profile.name.trim().to_owned();
        profile.host = profile.host.trim().to_owned();
        profile.username = profile.username.trim().to_owned();
        profile.database = profile.database.trim().to_owned();
        if profile.is_file() {
            // Nothing to keep in the keychain for a file.
            profile.save_password = false;
        }
        if let Err(message) = profile.validate(user) {
            self.error = Some(message);
            return None;
        }
        self.error = None;
        Some(profile)
    }
}

/// Draws the dialog. Returns `false` once it should close.
pub fn show(
    ctx: &egui::Context,
    form: &mut ConnectionForm,
    drivers: &[DriverChoice],
    actions: &mut Vec<Action>,
) -> bool {
    let chosen = drivers
        .iter()
        .find(|driver| driver.info.id == form.profile.driver);
    // A database file rather than a server (SQLite): no host, port, user or
    // password, but a path.
    let file_based = chosen.map_or(form.profile.is_file(), |driver| {
        driver.info.default_port == 0
    });
    let user_login = chosen.map_or(Login::Required, |driver| driver.info.user);
    let password_login = chosen.map_or(Login::Optional, |driver| driver.info.password);
    let palette = Palette::current(ctx);
    let mut open = true;
    let modal = egui::Modal::new(egui::Id::new("connection-form"))
        .frame(
            egui::Frame::new()
                .fill(palette.surface)
                .stroke(Stroke::new(1.0, palette.outline))
                .corner_radius(CornerRadius::same(14))
                .shadow(ctx.global_style().visuals.window_shadow)
                .inner_margin(egui::Margin::same(24)),
        )
        .show(ctx, |ui| {
            ui.set_width(540.0);
            ui.spacing_mut().item_spacing.y = 12.0;

            // Header.
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(36.0), Sense::hover());
                ui.painter()
                    .rect_filled(rect, CornerRadius::same(10), palette.accent_soft);
                widgets::paint_icon(ui, Icon::Database, rect.shrink(9.0), palette.accent);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    let title = if form.editing {
                        "Edit connection"
                    } else {
                        "New connection"
                    };
                    ui.label(
                        RichText::new(title)
                            .font(theme::semibold(17.0))
                            .color(palette.text),
                    );
                    let driver =
                        chosen.map_or("Unknown driver", |driver| driver.info.name.as_str());
                    ui.label(RichText::new(driver).color(palette.secondary));
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                    if widgets::icon_button(ui, Icon::Close, "Close").clicked() {
                        open = false;
                    }
                });
            });

            // Driver choice, when there is more than one.
            if drivers.len() > 1 && !form.editing {
                ui.horizontal_wrapped(|ui| {
                    for driver in drivers {
                        let selected = driver.info.id == form.profile.driver;
                        let tone = if selected {
                            Tone::Primary
                        } else {
                            Tone::Secondary
                        };
                        let icon = if driver.installed {
                            Icon::Database
                        } else {
                            Icon::Download
                        };
                        let response = widgets::button(ui, tone, Some(icon), &driver.info.name);
                        let response = if driver.installed {
                            response
                        } else {
                            response.on_hover_text("Downloaded when you connect")
                        };
                        if response.clicked() && !selected {
                            form.switch_driver(chosen.map(|c| &c.info), &driver.info);
                        }
                    }
                });
            }

            if !file_based {
                let mut fill = false;
                widgets::field(ui, "Connection string", |ui| {
                    ui.horizontal(|ui| {
                        let width = ui.available_width() - 70.0;
                        let masked = form.uri.contains('@');
                        let response = ui.add_sized(
                            [width, 30.0],
                            widgets::text_input(
                                &mut form.uri,
                                "Paste one, e.g. rediss://user:pass@host:6380/0",
                            )
                            .password(masked),
                        );
                        fill |= response.lost_focus()
                            && ui.input(|input| input.key_pressed(egui::Key::Enter));
                        fill |= widgets::button(ui, Tone::Secondary, Some(Icon::Link), "Fill")
                            .clicked();
                    })
                    .response
                });
                if fill && !form.uri.trim().is_empty() {
                    match violet_core::uri::parse(&form.uri) {
                        Ok(parsed) => {
                            match drivers.iter().find(|d| d.info.id.as_str() == parsed.driver) {
                                Some(driver)
                                    if form.editing && driver.info.id != form.profile.driver =>
                                {
                                    form.error = Some(format!(
                                        "That string is for {}; this connection is {}",
                                        driver.info.name,
                                        chosen.map_or("another database", |c| c.info.name.as_str())
                                    ));
                                }
                                Some(driver) => {
                                    if driver.info.id != form.profile.driver {
                                        form.switch_driver(chosen.map(|c| &c.info), &driver.info);
                                    }
                                    form.apply_uri(parsed, &driver.info);
                                }
                                None => {
                                    form.error =
                                        Some(format!("No {} provider is on offer", parsed.driver));
                                }
                            }
                        }
                        Err(message) => form.error = Some(message),
                    }
                }
            }

            widgets::field(ui, "Name", |ui| {
                ui.add(widgets::text_input(
                    &mut form.profile.name,
                    "Production, Local dev…",
                ))
            });

            if file_based {
                database_file(ui, form);
            } else {
                ui.columns(2, |columns| {
                    widgets::field(&mut columns[0], "Host", |ui| {
                        ui.add(widgets::text_input(&mut form.profile.host, "localhost"))
                    });
                    let port = chosen.map_or_else(
                        || "5432".to_owned(),
                        |driver| driver.info.default_port.to_string(),
                    );
                    widgets::field(&mut columns[1], "Port", |ui| {
                        ui.add(widgets::text_input(&mut form.port, &port))
                    });
                });
                let database = chosen
                    .map(|driver| driver.info.default_database.clone())
                    .filter(|d| !d.is_empty())
                    .unwrap_or_else(|| "postgres".to_owned());
                let user = chosen
                    .map(|driver| driver.info.default_user.clone())
                    .filter(|u| !u.is_empty())
                    .unwrap_or_else(|| {
                        if user_login == Login::Required {
                            "postgres".to_owned()
                        } else {
                            "default".to_owned()
                        }
                    });
                let database_label = match chosen.map(|d| d.info.language) {
                    Some(violet_core::Language::Redis) => "Database (0–15…)",
                    _ => "Database",
                };
                ui.columns(2, |columns| {
                    widgets::field(&mut columns[0], database_label, |ui| {
                        ui.add(widgets::text_input(&mut form.profile.database, &database))
                    });
                    if user_login != Login::None {
                        let label = if user_login == Login::Optional {
                            "User (optional)"
                        } else {
                            "User"
                        };
                        widgets::field(&mut columns[1], label, |ui| {
                            ui.add(widgets::text_input(&mut form.profile.username, &user))
                        });
                    }
                });

                let password_label =
                    if password_login == Login::Optional && user_login != Login::Required {
                        "Password (only if the server asks for one)"
                    } else {
                        "Password"
                    };
                if password_login != Login::None {
                    widgets::field(ui, password_label, |ui| {
                        ui.horizontal(|ui| {
                            let hint = if form.has_saved_password {
                                "Saved in keychain — type to replace"
                            } else {
                                "Password"
                            };
                            let width = ui.available_width() - 36.0;
                            ui.add_sized(
                                [width, 30.0],
                                widgets::text_input(&mut form.password, hint)
                                    .password(!form.show_password),
                            );
                            let (icon, tip) = if form.show_password {
                                (Icon::EyeOff, "Hide")
                            } else {
                                (Icon::Eye, "Show")
                            };
                            if widgets::icon_button(ui, icon, tip).clicked() {
                                form.show_password = !form.show_password;
                            }
                        })
                        .response
                    });
                    ui.horizontal(|ui| {
                        widgets::checkbox(
                            ui,
                            &mut form.profile.save_password,
                            "Save password in the system keychain",
                        );
                    });
                }
            }
            if chosen.is_some_and(|driver| driver.info.capabilities.databases) {
                ui.horizontal(|ui| {
                    widgets::checkbox(
                        ui,
                        &mut form.profile.show_all_databases,
                        "Show all databases",
                    )
                    .on_hover_text(
                        "List every database on the server in the tree, not only this one",
                    );
                });
            }

            let tls = !file_based && chosen.is_some_and(|driver| driver.info.capabilities.tls);
            ui.horizontal(|ui| {
                if tls {
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 5.0;
                        ui.label(
                            RichText::new("SSL")
                                .font(theme::medium(12.0))
                                .color(palette.secondary),
                        );
                        let options: Vec<(SslMode, &str)> = SslMode::ALL
                            .iter()
                            .map(|mode| (*mode, mode.label()))
                            .collect();
                        widgets::segmented(ui, &mut form.profile.ssl, &options);
                    });
                    ui.add_space(16.0);
                }
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 5.0;
                    ui.label(
                        RichText::new("Color")
                            .font(theme::medium(12.0))
                            .color(palette.secondary),
                    );
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        swatch(ui, &mut form.profile.color, None, &palette);
                        for tag in theme::TAGS {
                            swatch(ui, &mut form.profile.color, Some(tag), &palette);
                        }
                    });
                });
            });
            if form.profile.ssl == SslMode::Disable && tls && !is_loopback(&form.profile.host) {
                notice(
                    ui,
                    &palette,
                    Icon::Lock,
                    palette.warning,
                    "Without SSL, the password and data cross the network unencrypted.",
                );
            }

            if let Some(driver) = chosen.filter(|driver| !driver.installed && !driver.installing) {
                notice(
                    ui,
                    &palette,
                    Icon::Download,
                    palette.accent,
                    &format!(
                        "The {} provider ({}) is downloaded the first time you connect. \
                         It runs sandboxed and can only reach the server above.",
                        driver.info.name,
                        format_size(driver.size)
                    ),
                );
            }

            // Test result or validation error.
            match &form.test {
                TestState::Idle => {}
                TestState::Running(_) => {
                    let text = match chosen.filter(|driver| driver.installing) {
                        Some(driver) => format!("Downloading the {} provider…", driver.info.name),
                        None => "Testing connection…".to_owned(),
                    };
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(14.0).color(palette.accent));
                        ui.label(RichText::new(text).color(palette.secondary));
                    });
                }
                TestState::Passed(server) => notice(
                    ui,
                    &palette,
                    Icon::CircleCheck,
                    palette.success,
                    &format!(
                        "Connected to {} · {} as {}",
                        server.version, server.database, server.user
                    ),
                ),
                TestState::Failed(message) => {
                    notice(ui, &palette, Icon::CircleAlert, palette.danger, message)
                }
            }
            if let Some(error) = &form.error {
                notice(ui, &palette, Icon::CircleAlert, palette.danger, error);
            }

            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let testing = matches!(form.test, TestState::Running(_));
                if ui
                    .add_enabled_ui(!testing, |ui| {
                        widgets::button(ui, Tone::Secondary, Some(Icon::Zap), "Test")
                    })
                    .inner
                    .clicked()
                    && let Some(profile) = form.finish(user_login)
                {
                    actions.push(Action::TestProfile {
                        profile,
                        password: form.typed_password(),
                    });
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if widgets::button(ui, Tone::Primary, Some(Icon::Plug), "Save & connect")
                        .clicked()
                        && let Some(profile) = form.finish(user_login)
                    {
                        actions.push(Action::SaveProfile {
                            profile,
                            password: form.typed_password(),
                            connect: true,
                        });
                        open = false;
                    }
                    if widgets::button(ui, Tone::Secondary, None, "Save").clicked()
                        && let Some(profile) = form.finish(user_login)
                    {
                        actions.push(Action::SaveProfile {
                            profile,
                            password: form.typed_password(),
                            connect: false,
                        });
                        open = false;
                    }
                    if widgets::button(ui, Tone::Ghost, None, "Cancel").clicked() {
                        open = false;
                    }
                });
            });
        });
    if modal.should_close() {
        open = false;
    }
    open
}

/// File extensions SQLite databases go by.
const DATABASE_FILES: [&str; 5] = ["db", "sqlite", "sqlite3", "db3", "s3db"];

/// The database file field: its path, picked or typed, and whether it
/// opens read-only.
fn database_file(ui: &mut egui::Ui, form: &mut ConnectionForm) {
    widgets::field(ui, "Database file", |ui| {
        ui.horizontal(|ui| {
            let width = ui.available_width() - 190.0;
            ui.add_sized(
                [width.max(120.0), 30.0],
                widgets::text_input(&mut form.profile.host, "Path to a .db or .sqlite file"),
            );
            let start = std::path::Path::new(form.profile.host.trim())
                .parent()
                .filter(|folder| folder.is_dir())
                .map(std::path::Path::to_path_buf);
            let picker = || {
                let picker = rfd::FileDialog::new()
                    .add_filter("SQLite database", &DATABASE_FILES)
                    .add_filter("All files", &["*"]);
                match &start {
                    Some(folder) => picker.set_directory(folder),
                    None => picker,
                }
            };
            if widgets::button(ui, Tone::Secondary, Some(Icon::FolderOpen), "Open…")
                .on_hover_text("Pick an existing database file")
                .clicked()
                && let Some(path) = picker().set_title("Open a SQLite database").pick_file()
            {
                form.profile.host = path.to_string_lossy().into_owned();
            }
            if widgets::button(ui, Tone::Secondary, Some(Icon::Plus), "New…")
                .on_hover_text("Create a new, empty database file")
                .clicked()
                && let Some(path) = picker()
                    .set_title("Create a SQLite database")
                    .set_file_name("database.db")
                    .save_file()
            {
                form.profile.host = path.to_string_lossy().into_owned();
            }
        })
        .response
    });
    let mut read_only = form.profile.options.get("read_only").map(String::as_str) == Some("true");
    if widgets::checkbox(ui, &mut read_only, "Read only")
        .on_hover_text("Open the database without changing it")
        .changed()
    {
        if read_only {
            form.profile
                .options
                .insert("read_only".to_owned(), "true".to_owned());
        } else {
            form.profile.options.remove("read_only");
        }
    }
}

fn swatch(ui: &mut egui::Ui, value: &mut Option<[u8; 3]>, tag: Option<[u8; 3]>, palette: &Palette) {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(22.0), Sense::click());
    let selected = *value == tag;
    let center = rect.center();
    match tag {
        Some(rgb) => {
            ui.painter().circle_filled(center, 8.0, theme::tag(rgb));
        }
        None => {
            ui.painter()
                .circle_stroke(center, 7.5, Stroke::new(1.5, palette.dim));
            ui.painter().line_segment(
                [
                    center + egui::vec2(-5.0, 5.0),
                    center + egui::vec2(5.0, -5.0),
                ],
                Stroke::new(1.5, palette.dim),
            );
        }
    }
    if selected {
        ui.painter()
            .circle_stroke(center, 10.5, Stroke::new(2.0, palette.text));
    } else if response.hovered() {
        ui.painter()
            .circle_stroke(center, 10.5, Stroke::new(1.0, palette.dim));
    }
    if response
        .on_hover_text(if tag.is_some() {
            "Tag colour"
        } else {
            "No colour"
        })
        .clicked()
    {
        *value = tag;
    }
}

fn notice(ui: &mut egui::Ui, palette: &Palette, icon: Icon, color: egui::Color32, text: &str) {
    egui::Frame::new()
        .fill(color.gamma_multiply(if palette.dark { 0.1 } else { 0.08 }))
        .stroke(Stroke::new(1.0, color.gamma_multiply(0.35)))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(egui::Margin::symmetric(12, 9))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(16.0), Sense::hover());
                widgets::paint_icon(ui, icon, rect, color);
                ui.add(egui::Label::new(RichText::new(text).color(palette.text)).wrap());
            });
        });
}

/// Asks for a password the profile does not keep.
pub struct PasswordPrompt {
    pub id: violet_core::ConnectionId,
    pub name: String,
    pub password: String,
}

impl Drop for PasswordPrompt {
    fn drop(&mut self) {
        self.password.zeroize();
    }
}

/// Returns `false` once the prompt should close.
pub fn show_prompt(
    ctx: &egui::Context,
    prompt: &mut PasswordPrompt,
    actions: &mut Vec<Action>,
) -> bool {
    let palette = Palette::current(ctx);
    let mut open = true;
    let modal = egui::Modal::new(egui::Id::new("password-prompt"))
        .frame(
            egui::Frame::new()
                .fill(palette.surface)
                .stroke(Stroke::new(1.0, palette.outline))
                .corner_radius(CornerRadius::same(14))
                .inner_margin(egui::Margin::same(22)),
        )
        .show(ctx, |ui| {
            ui.set_width(380.0);
            ui.spacing_mut().item_spacing.y = 12.0;
            ui.label(
                RichText::new("Password required")
                    .font(theme::semibold(16.0))
                    .color(palette.text),
            );
            ui.label(
                RichText::new(format!("Enter the password for {}.", prompt.name))
                    .color(palette.secondary),
            );
            let response =
                ui.add(widgets::text_input(&mut prompt.password, "Password").password(true));
            response.request_focus();
            let submit =
                response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if widgets::button(ui, Tone::Primary, Some(Icon::Plug), "Connect").clicked()
                        || submit
                    {
                        actions.push(Action::ConnectWithPassword(
                            prompt.id,
                            Secret::new(prompt.password.clone()),
                        ));
                        open = false;
                    }
                    if widgets::button(ui, Tone::Ghost, None, "Cancel").clicked() {
                        open = false;
                    }
                });
            });
        });
    open && !modal.should_close()
}

/// Asks before deleting a connection. Returns `Some(true)` to delete,
/// `Some(false)` to cancel, `None` while open.
pub fn confirm_delete(ctx: &egui::Context, name: &str) -> Option<bool> {
    let palette = Palette::current(ctx);
    let mut answer = None;
    let modal = egui::Modal::new(egui::Id::new("confirm-delete"))
        .frame(
            egui::Frame::new()
                .fill(palette.surface)
                .stroke(Stroke::new(1.0, palette.outline))
                .corner_radius(CornerRadius::same(14))
                .inner_margin(egui::Margin::same(22)),
        )
        .show(ctx, |ui| {
            ui.set_width(380.0);
            ui.spacing_mut().item_spacing.y = 12.0;
            ui.label(RichText::new("Delete connection?").font(theme::semibold(16.0)).color(palette.text));
            ui.add(
                egui::Label::new(
                    RichText::new(format!(
                        "\u{201C}{name}\u{201D} and its saved password will be removed. The database itself is not touched."
                    ))
                    .color(palette.secondary),
                )
                .wrap(),
            );
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if widgets::button(ui, Tone::Danger, Some(Icon::Trash), "Delete").clicked() {
                        answer = Some(true);
                    }
                    if widgets::button(ui, Tone::Ghost, None, "Cancel").clicked() {
                        answer = Some(false);
                    }
                });
            });
        });
    if modal.should_close() {
        answer = Some(false);
    }
    answer
}
