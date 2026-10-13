//! Short-lived notices in the bottom-right corner.

use egui::{Align2, CornerRadius, Id, Sense, Stroke, Vec2};

use super::icons::Icon;
use super::theme::{self, Palette};
use super::widgets;

const LIFETIME: f64 = 5.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Success,
    Error,
}

#[derive(Clone, Debug)]
pub struct Toast {
    pub kind: ToastKind,
    pub title: String,
    pub body: Option<String>,
}

impl Toast {
    pub fn info(title: impl Into<String>) -> Self {
        Self {
            kind: ToastKind::Info,
            title: title.into(),
            body: None,
        }
    }

    pub fn success(title: impl Into<String>) -> Self {
        Self {
            kind: ToastKind::Success,
            title: title.into(),
            body: None,
        }
    }

    pub fn error(title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            kind: ToastKind::Error,
            title: title.into(),
            body: Some(body.into()),
        }
    }
}

struct Shown {
    toast: Toast,
    id: u64,
    born: f64,
}

#[derive(Default)]
pub struct Toasts {
    shown: Vec<Shown>,
    next: u64,
}

impl Toasts {
    pub fn push(&mut self, ctx: &egui::Context, toast: Toast) {
        self.next += 1;
        let born = ctx.input(|input| input.time);
        self.shown.push(Shown {
            toast,
            id: self.next,
            born,
        });
        // Four at once is plenty; the oldest go first.
        if self.shown.len() > 4 {
            self.shown.remove(0);
        }
    }

    pub fn show(&mut self, ctx: &egui::Context) {
        let now = ctx.input(|input| input.time);
        let palette = Palette::current(ctx);
        let mut dismissed = Vec::new();
        // Above the status bar and a result's paging footer, whose buttons
        // would otherwise sit under the newest notice.
        let mut offset = 12.0 + 26.0 + 34.0;
        for shown in self.shown.iter().rev() {
            let age = now - shown.born;
            let hovered_id = Id::new(("toast-hover", shown.id));
            let hovered = ctx.data(|data| data.get_temp::<bool>(hovered_id).unwrap_or(false));
            if age > LIFETIME && !hovered {
                dismissed.push(shown.id);
                continue;
            }
            let (icon, tint) = match shown.toast.kind {
                ToastKind::Info => (Icon::Info, palette.accent),
                ToastKind::Success => (Icon::CircleCheck, palette.success),
                ToastKind::Error => (Icon::CircleAlert, palette.danger),
            };
            let response = egui::Area::new(Id::new(("toast", shown.id)))
                .anchor(Align2::RIGHT_BOTTOM, Vec2::new(-16.0, -offset))
                .order(egui::Order::Tooltip)
                .show(ctx, |ui| {
                    egui::Frame::new()
                        .fill(palette.surface)
                        .stroke(Stroke::new(1.0, palette.outline))
                        .corner_radius(CornerRadius::same(10))
                        .shadow(ui.visuals().popup_shadow)
                        .inner_margin(egui::Margin::symmetric(14, 12))
                        .show(ui, |ui| {
                            ui.set_width(320.0);
                            ui.horizontal_top(|ui| {
                                let (rect, _) =
                                    ui.allocate_exact_size(Vec2::splat(18.0), Sense::hover());
                                widgets::paint_icon(ui, icon, rect, tint);
                                ui.vertical(|ui| {
                                    ui.spacing_mut().item_spacing.y = 3.0;
                                    ui.label(
                                        egui::RichText::new(&shown.toast.title)
                                            .font(theme::semibold(13.0))
                                            .color(palette.text),
                                    );
                                    if let Some(body) = &shown.toast.body {
                                        ui.add(
                                            egui::Label::new(
                                                egui::RichText::new(body).color(palette.secondary),
                                            )
                                            .wrap(),
                                        );
                                    }
                                });
                            });
                        })
                })
                .response;
            let hovering = response.contains_pointer();
            ctx.data_mut(|data| data.insert_temp(hovered_id, hovering));
            if response.interact(Sense::click()).clicked() {
                dismissed.push(shown.id);
            }
            offset += response.rect.height() + 8.0;
        }
        self.shown.retain(|shown| !dismissed.contains(&shown.id));
        if !self.shown.is_empty() {
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        }
    }
}
