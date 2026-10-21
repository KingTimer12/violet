//! Database providers: what is installed, what the catalogue offers, and
//! the dialog that manages them.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use egui::{CornerRadius, RichText, Sense, Stroke, Vec2};
use violet_core::ports::ProviderListing;
use violet_core::{DriverId, DriverInfo};

use super::action::Action;
use super::icons::Icon;
use super::theme::{self, Palette};
use super::widgets::{self, Tone};

/// The catalogue, as last fetched.
#[derive(Debug, Default)]
pub enum Listing {
    #[default]
    Unknown,
    Loading,
    Ready(Vec<ProviderListing>),
    Failed(String),
}

/// What the interface knows about providers.
#[derive(Debug, Default)]
pub struct Providers {
    pub listing: Listing,
    /// Being installed, updated or removed.
    pub busy: HashSet<DriverId>,
    /// The last failure per provider, shown in the dialog.
    pub errors: HashMap<DriverId, String>,
    /// The dialog is open.
    pub open: bool,
    /// "New connection" waits for the catalogue to offer a driver.
    pub awaiting_new_connection: bool,
}

/// A database the connection form can offer.
#[derive(Clone, Debug)]
pub struct DriverChoice {
    pub info: DriverInfo,
    pub installed: bool,
    /// Download size, when not installed.
    pub size: u64,
    pub installing: bool,
}

impl Providers {
    /// Installed drivers first-hand, then whatever else the catalogue
    /// offers, by name.
    pub fn choices(&self, installed: Vec<DriverInfo>) -> Vec<DriverChoice> {
        let mut choices: Vec<DriverChoice> = installed
            .into_iter()
            .map(|info| DriverChoice {
                installing: self.busy.contains(&info.id),
                info,
                installed: true,
                size: 0,
            })
            .collect();
        if let Listing::Ready(listings) = &self.listing {
            for listing in listings {
                if !choices.iter().any(|c| c.info.id == listing.info.id) {
                    choices.push(DriverChoice {
                        info: listing.info.clone(),
                        installed: false,
                        size: listing.size,
                        installing: self.busy.contains(&listing.info.id),
                    });
                }
            }
        }
        choices.sort_by(|a, b| a.info.name.cmp(&b.info.name));
        choices
    }

    /// A provider's name, for messages.
    pub fn name(&self, id: &DriverId) -> String {
        match &self.listing {
            Listing::Ready(listings) => listings
                .iter()
                .find(|listing| listing.info.id == *id)
                .map(|listing| listing.info.name.clone()),
            _ => None,
        }
        .unwrap_or_else(|| id.to_string())
    }
}

/// `1.2 MB`, `380 KB`.
pub fn format_size(bytes: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let bytes = bytes as f64;
    if bytes >= 1_000_000.0 {
        format!("{:.1} MB", bytes / 1_000_000.0)
    } else {
        format!("{:.0} KB", (bytes / 1_000.0).max(1.0))
    }
}

/// Draws the dialog. Returns `false` once it should close.
pub fn show(
    ctx: &egui::Context,
    providers: &Providers,
    folder: &Path,
    actions: &mut Vec<Action>,
) -> bool {
    let palette = Palette::current(ctx);
    let mut open = true;
    let modal = egui::Modal::new(egui::Id::new("providers"))
        .frame(
            egui::Frame::new()
                .fill(palette.surface)
                .stroke(Stroke::new(1.0, palette.outline))
                .corner_radius(CornerRadius::same(14))
                .shadow(ctx.global_style().visuals.window_shadow)
                .inner_margin(egui::Margin::same(24)),
        )
        .show(ctx, |ui| {
            ui.set_width(560.0);
            ui.spacing_mut().item_spacing.y = 12.0;

            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(36.0), Sense::hover());
                ui.painter()
                    .rect_filled(rect, CornerRadius::same(10), palette.accent_soft);
                widgets::paint_icon(ui, Icon::Package, rect.shrink(9.0), palette.accent);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.label(
                        RichText::new("Database providers")
                            .font(theme::semibold(17.0))
                            .color(palette.text),
                    );
                    ui.label(
                        RichText::new("Downloaded when a connection first needs one")
                            .color(palette.secondary),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                    if widgets::icon_button(ui, Icon::Close, "Close").clicked() {
                        open = false;
                    }
                    let loading = matches!(providers.listing, Listing::Loading);
                    if ui
                        .add_enabled_ui(!loading, |ui| {
                            widgets::icon_button(ui, Icon::Refresh, "Check for updates")
                        })
                        .inner
                        .clicked()
                    {
                        actions.push(Action::RefreshProviders);
                    }
                });
            });

            match &providers.listing {
                Listing::Unknown | Listing::Loading => {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(14.0).color(palette.accent));
                        ui.label(RichText::new("Loading the catalogue…").color(palette.secondary));
                    });
                }
                Listing::Failed(message) => {
                    ui.add(
                        egui::Label::new(
                            RichText::new(format!("The catalogue could not be loaded: {message}"))
                                .color(palette.danger),
                        )
                        .wrap(),
                    );
                }
                Listing::Ready(listings) if listings.is_empty() => {
                    ui.label(RichText::new("The catalogue is empty.").color(palette.secondary));
                }
                Listing::Ready(listings) => {
                    egui::ScrollArea::vertical()
                        .max_height(360.0)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            for listing in listings {
                                row(ui, &palette, providers, listing, actions);
                            }
                        });
                }
            }

            ui.add(
                egui::Label::new(
                    RichText::new(format!(
                        "Providers run sandboxed: they can only reach the server of the \
                         connection using them. Installed in {}",
                        folder.display()
                    ))
                    .size(11.5)
                    .color(palette.dim),
                )
                .wrap(),
            );
        });
    open && !modal.should_close()
}

fn row(
    ui: &mut egui::Ui,
    palette: &Palette,
    providers: &Providers,
    listing: &ProviderListing,
    actions: &mut Vec<Action>,
) {
    let id = &listing.info.id;
    egui::Frame::new()
        .fill(palette.window)
        .stroke(Stroke::new(1.0, palette.outline))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(egui::Margin::symmetric(14, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(20.0), Sense::hover());
                widgets::paint_icon(ui, Icon::for_driver(id.as_str()), rect, palette.accent);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 3.0;
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(&listing.info.name)
                                .font(theme::semibold(14.0))
                                .color(palette.text),
                        );
                        let version = match &listing.installed {
                            Some(installed) if listing.outdated() => {
                                format!("{installed} → {}", listing.version)
                            }
                            Some(installed) => installed.clone(),
                            None => listing.version.clone(),
                        };
                        ui.label(RichText::new(version).color(palette.dim));
                    });
                    if !listing.description.is_empty() {
                        ui.label(RichText::new(&listing.description).color(palette.secondary));
                    }
                    if let Some(error) = providers.errors.get(id) {
                        ui.add(egui::Label::new(RichText::new(error).color(palette.danger)).wrap());
                    }
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if providers.busy.contains(id) {
                        ui.add(egui::Spinner::new().size(16.0).color(palette.accent));
                        return;
                    }
                    match &listing.installed {
                        None => {
                            let label = format!("Install · {}", format_size(listing.size));
                            if widgets::button(ui, Tone::Primary, Some(Icon::Download), &label)
                                .clicked()
                            {
                                actions.push(Action::InstallProvider(id.clone()));
                            }
                        }
                        Some(_) => {
                            if widgets::icon_button(ui, Icon::Trash, "Remove").clicked() {
                                actions.push(Action::RemoveProvider(id.clone()));
                            }
                            if listing.outdated() {
                                if widgets::button(
                                    ui,
                                    Tone::Primary,
                                    Some(Icon::Download),
                                    "Update",
                                )
                                .clicked()
                                {
                                    actions.push(Action::InstallProvider(id.clone()));
                                }
                            } else {
                                widgets::pill(
                                    ui,
                                    "Installed",
                                    palette.success,
                                    palette.success.gamma_multiply(0.12),
                                );
                            }
                        }
                    }
                });
            });
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_naturally() {
        assert_eq!(format_size(387_836), "388 KB");
        assert_eq!(format_size(1_250_000), "1.2 MB");
        assert_eq!(format_size(10), "1 KB");
    }
}
