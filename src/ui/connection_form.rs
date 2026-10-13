//! The dialog that creates and edits connections.

use egui::{CornerRadius, RichText, Sense, Stroke, Vec2};
use violet_app::RequestId;
use violet_core::{ConnectionProfile, DriverInfo, Secret, ServerInfo, SslMode, is_loopback};
use zeroize::Zeroize as _;

use super::action::Action;
use super::icons::Icon;
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
}

impl Drop for ConnectionForm {
    fn drop(&mut self) {
        self.password.zeroize();
    }
}

impl ConnectionForm {
    pub fn create(driver: &DriverInfo) -> Self {
        let mut profile = ConnectionProfile::new(driver.id.clone(), driver.default_port);
        profile.database = driver.default_database.to_owned();
        profile.username = driver.default_user.to_owned();
        Self::with(profile, false, false)
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
        }
    }

    /// The password typed, if any. Empty means "keep the saved one".
    fn typed_password(&self) -> Option<Secret> {
        (!self.password.is_empty()).then(|| Secret::new(self.password.clone()))
    }

    /// Checks the form and returns the profile it describes.
    fn finish(&mut self) -> Option<ConnectionProfile> {
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
        if let Err(message) = profile.validate() {
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
    drivers: &[DriverInfo],
    actions: &mut Vec<Action>,
) -> bool {
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
                    let driver = drivers
                        .iter()
                        .find(|driver| driver.id == form.profile.driver)
                        .map_or("Unknown driver", |driver| driver.name);
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
                        let selected = driver.id == form.profile.driver;
                        let tone = if selected {
                            Tone::Primary
                        } else {
                            Tone::Secondary
                        };
                        if widgets::button(ui, tone, Some(Icon::Database), driver.name).clicked()
                            && !selected
                        {
                            form.profile.driver = driver.id.clone();
                            form.port = driver.default_port.to_string();
                        }
                    }
                });
            }

            widgets::field(ui, "Name", |ui| {
                ui.add(widgets::text_input(
                    &mut form.profile.name,
                    "Production, Local dev…",
                ))
            });

            ui.columns(2, |columns| {
                widgets::field(&mut columns[0], "Host", |ui| {
                    ui.add(widgets::text_input(&mut form.profile.host, "localhost"))
                });
                widgets::field(&mut columns[1], "Port", |ui| {
                    ui.add(widgets::text_input(&mut form.port, "5432"))
                });
            });
            ui.columns(2, |columns| {
                widgets::field(&mut columns[0], "Database", |ui| {
                    ui.add(widgets::text_input(&mut form.profile.database, "postgres"))
                });
                widgets::field(&mut columns[1], "User", |ui| {
                    ui.add(widgets::text_input(&mut form.profile.username, "postgres"))
                });
            });

            widgets::field(ui, "Password", |ui| {
                ui.horizontal(|ui| {
                    let hint = if form.has_saved_password {
                        "Saved in keychain — type to replace"
                    } else {
                        "Password"
                    };
                    let width = ui.available_width() - 36.0;
                    ui.add_sized(
                        [width, 30.0],
                        widgets::text_input(&mut form.password, hint).password(!form.show_password),
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
                ui.checkbox(
                    &mut form.profile.save_password,
                    "Save password in the system keychain",
                );
            });

            let tls = drivers
                .iter()
                .find(|driver| driver.id == form.profile.driver)
                .is_some_and(|driver| driver.capabilities.tls);
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

            // Test result or validation error.
            match &form.test {
                TestState::Idle => {}
                TestState::Running(_) => {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(14.0).color(palette.accent));
                        ui.label(RichText::new("Testing connection…").color(palette.secondary));
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
                    && let Some(profile) = form.finish()
                {
                    actions.push(Action::TestProfile {
                        profile,
                        password: form.typed_password(),
                    });
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if widgets::button(ui, Tone::Primary, Some(Icon::Plug), "Save & connect")
                        .clicked()
                        && let Some(profile) = form.finish()
                    {
                        actions.push(Action::SaveProfile {
                            profile,
                            password: form.typed_password(),
                            connect: true,
                        });
                        open = false;
                    }
                    if widgets::button(ui, Tone::Secondary, None, "Save").clicked()
                        && let Some(profile) = form.finish()
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
