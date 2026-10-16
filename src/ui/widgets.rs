//! Small widgets shared by every view, drawn from the palette.

use egui::text::{LayoutJob, TextWrapping};
use egui::{
    Align2, Color32, CornerRadius, FontId, Galley, Response, Sense, Stroke, StrokeKind, Ui, Vec2,
};
use std::sync::Arc;

use super::icons::Icon;
use super::theme::{self, Palette};

/// Paints `icon` centred in `rect`.
pub fn paint_icon(ui: &Ui, icon: Icon, rect: egui::Rect, tint: Color32) {
    icon.image(tint, rect.width()).paint_at(ui, rect);
}

/// A square button showing only an icon, with a tooltip.
pub fn icon_button(ui: &mut Ui, icon: Icon, tooltip: &str) -> Response {
    icon_button_sized(ui, icon, tooltip, 28.0, 16.0)
}

pub fn icon_button_sized(
    ui: &mut Ui,
    icon: Icon,
    tooltip: &str,
    size: f32,
    icon_size: f32,
) -> Response {
    let palette = Palette::current(ui.ctx());
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    if ui.is_rect_visible(rect) {
        let enabled = ui.is_enabled();
        let hovered = response.hovered() && enabled;
        if hovered || response.is_pointer_button_down_on() {
            let fill = if response.is_pointer_button_down_on() {
                palette.surface_active
            } else {
                palette.surface_hover
            };
            ui.painter().rect_filled(rect, CornerRadius::same(6), fill);
        }
        let tint = if !enabled {
            palette.dim.gamma_multiply(0.6)
        } else if hovered {
            palette.text
        } else {
            palette.secondary
        };
        paint_icon(
            ui,
            icon,
            egui::Rect::from_center_size(rect.center(), Vec2::splat(icon_size)),
            tint,
        );
    }
    response.on_hover_text(tooltip)
}

/// How a [`button`] is filled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    /// The one thing to do: accent fill.
    Primary,
    /// An outlined surface.
    Secondary,
    /// No fill until hovered.
    Ghost,
    /// Destructive.
    Danger,
}

/// A labelled button, optionally with a leading icon.
pub fn button(ui: &mut Ui, tone: Tone, icon: Option<Icon>, label: &str) -> Response {
    let palette = Palette::current(ui.ctx());
    let font = theme::medium(13.0);
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), font, Color32::PLACEHOLDER);
    let icon_size = 15.0;
    let gap = if icon.is_some() && !label.is_empty() {
        7.0
    } else {
        0.0
    };
    let icon_width = if icon.is_some() { icon_size } else { 0.0 };
    let padding = egui::vec2(12.0, 0.0);
    let size = egui::vec2(icon_width + gap + galley.size().x + padding.x * 2.0, 30.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    if !ui.is_rect_visible(rect) {
        return response;
    }
    let enabled = ui.is_enabled();
    let hovered = response.hovered() && enabled;
    let pressed = response.is_pointer_button_down_on() && enabled;
    let (fill, stroke, text) = match tone {
        Tone::Primary => (
            if hovered {
                palette.accent_hover
            } else {
                palette.accent
            },
            Color32::TRANSPARENT,
            palette.on_accent,
        ),
        Tone::Secondary => (
            if pressed {
                palette.surface_active
            } else if hovered {
                palette.surface_hover
            } else {
                palette.surface
            },
            palette.outline,
            palette.text,
        ),
        Tone::Ghost => (
            if hovered {
                palette.surface_hover
            } else {
                Color32::TRANSPARENT
            },
            Color32::TRANSPARENT,
            palette.text,
        ),
        Tone::Danger => (
            if hovered {
                palette.danger
            } else {
                palette.danger.gamma_multiply(0.85)
            },
            Color32::TRANSPARENT,
            Color32::WHITE,
        ),
    };
    let (fill, text) = if enabled {
        (fill, text)
    } else {
        (fill.gamma_multiply(0.5), text.gamma_multiply(0.5))
    };
    let painter = ui.painter();
    painter.rect(
        rect,
        CornerRadius::same(7),
        fill,
        Stroke::new(1.0, stroke),
        StrokeKind::Inside,
    );
    let mut x = rect.left() + padding.x;
    if let Some(icon) = icon {
        let icon_rect = egui::Rect::from_min_size(
            egui::pos2(x, rect.center().y - icon_size / 2.0),
            Vec2::splat(icon_size),
        );
        paint_icon(ui, icon, icon_rect, text);
        x += icon_size + gap;
    }
    painter.galley(
        egui::pos2(x, rect.center().y - galley.size().y / 2.0),
        galley,
        text,
    );
    response
}

/// A single-line galley that ends in `…` when wider than `width`.
pub fn elided(ui: &Ui, text: &str, font: FontId, color: Color32, width: f32) -> Arc<Galley> {
    let mut job = LayoutJob::single_section(text.to_owned(), egui::TextFormat::simple(font, color));
    job.wrap = TextWrapping {
        max_width: width.max(1.0),
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some('…'),
    };
    ui.painter().layout_job(job)
}

/// An uppercase caption above a group of things.
pub fn caption(ui: &mut Ui, text: &str) {
    let palette = Palette::current(ui.ctx());
    ui.label(
        egui::RichText::new(text.to_uppercase())
            .font(theme::semibold(10.5))
            .color(palette.dim)
            .extra_letter_spacing(0.6),
    );
}

/// A small rounded label, such as a row count.
pub fn pill(ui: &mut Ui, text: &str, fg: Color32, bg: Color32) -> Response {
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_owned(), theme::medium(11.0), fg);
    let size = galley.size() + egui::vec2(14.0, 6.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(255), bg);
    ui.painter()
        .galley(rect.center() - galley.size() / 2.0, galley, fg);
    response
}

/// A coloured dot, for connection status.
pub fn dot(ui: &mut Ui, color: Color32, radius: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(radius * 2.0 + 4.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), radius, color);
}

/// A labelled text field in a form: the label above, the field filling the
/// width.
pub fn field(ui: &mut Ui, label: &str, add: impl FnOnce(&mut Ui) -> Response) -> Response {
    let palette = Palette::current(ui.ctx());
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 5.0;
        ui.label(
            egui::RichText::new(label)
                .font(theme::medium(12.0))
                .color(palette.secondary),
        );
        add(ui)
    })
    .inner
}

/// A text input styled for forms.
pub fn text_input<'t>(text: &'t mut dyn egui::TextBuffer, hint: &str) -> egui::TextEdit<'t> {
    egui::TextEdit::singleline(text)
        .hint_text(hint)
        .margin(egui::vec2(10.0, 7.0))
        .desired_width(f32::INFINITY)
}

/// A row of mutually exclusive options.
pub fn segmented<T: PartialEq + Copy>(ui: &mut Ui, value: &mut T, options: &[(T, &str)]) -> bool {
    let palette = Palette::current(ui.ctx());
    let mut changed = false;
    egui::Frame::new()
        .fill(palette.panel)
        .stroke(Stroke::new(1.0, palette.outline))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(egui::Margin::same(3))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                for (option, label) in options {
                    let selected = *value == *option;
                    let galley = ui.painter().layout_no_wrap(
                        (*label).to_owned(),
                        theme::medium(12.5),
                        Color32::PLACEHOLDER,
                    );
                    let size = egui::vec2(galley.size().x + 22.0, 26.0);
                    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
                    let fill = if selected {
                        palette.surface_active
                    } else if response.hovered() {
                        palette.surface_hover
                    } else {
                        Color32::TRANSPARENT
                    };
                    ui.painter().rect_filled(rect, CornerRadius::same(6), fill);
                    let color = if selected {
                        palette.text
                    } else {
                        palette.secondary
                    };
                    ui.painter()
                        .galley(rect.center() - galley.size() / 2.0, galley, color);
                    if response.clicked() && !selected {
                        *value = *option;
                        changed = true;
                    }
                }
            });
        });
    changed
}

/// Text centred in the available space, for empty states.
pub fn empty_state(ui: &mut Ui, icon: Icon, title: &str, body: &str) {
    let palette = Palette::current(ui.ctx());
    ui.vertical_centered(|ui| {
        ui.add_space((ui.available_height() / 2.0 - 60.0).max(12.0));
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(40.0), Sense::hover());
        ui.painter()
            .rect_filled(rect, CornerRadius::same(12), palette.accent_soft);
        paint_icon(
            ui,
            icon,
            egui::Rect::from_center_size(rect.center(), Vec2::splat(20.0)),
            palette.accent,
        );
        ui.add_space(10.0);
        ui.label(
            egui::RichText::new(title)
                .font(theme::semibold(14.0))
                .color(palette.text),
        );
        ui.label(egui::RichText::new(body).color(palette.dim));
    });
}

/// A single line of text painted in a rect, elided to fit.
pub fn paint_text(
    ui: &Ui,
    rect: egui::Rect,
    align: Align2,
    text: &str,
    font: FontId,
    color: Color32,
) {
    let galley = elided(ui, text, font, color, rect.width());
    let pos = align.align_size_within_rect(galley.size(), rect).min;
    ui.painter().with_clip_rect(rect).galley(pos, galley, color);
}

/// A checkbox as JetBrains IDEs draw one: a 16 px square with softly
/// rounded corners, filled with the accent and a white tick when on.
/// `indeterminate` draws a dash, for a choice only partly made.
pub struct Check<'a> {
    checked: &'a mut bool,
    text: egui::WidgetText,
    indeterminate: bool,
}

impl<'a> Check<'a> {
    pub fn new(checked: &'a mut bool, text: impl Into<egui::WidgetText>) -> Self {
        Self {
            checked,
            text: text.into(),
            indeterminate: false,
        }
    }

    pub fn indeterminate(mut self, indeterminate: bool) -> Self {
        self.indeterminate = indeterminate;
        self
    }
}

impl egui::Widget for Check<'_> {
    fn ui(self, ui: &mut Ui) -> Response {
        const BOX: f32 = 16.0;
        const GAP: f32 = 8.0;
        let palette = Palette::current(ui.ctx());
        let Self {
            checked,
            text,
            indeterminate,
        } = self;
        let has_text = !text.is_empty();
        let galley = text.into_galley(
            ui,
            Some(egui::TextWrapMode::Extend),
            f32::INFINITY,
            egui::TextStyle::Body,
        );
        let width = if has_text {
            BOX + GAP + galley.size().x
        } else {
            BOX
        };
        let size = egui::vec2(width, galley.size().y.max(BOX) + 4.0);
        let (rect, mut response) = ui.allocate_exact_size(size, Sense::click());
        if response.clicked() {
            *checked = !*checked;
            response.mark_changed();
        }
        let enabled = ui.is_enabled();
        response.widget_info(|| {
            egui::WidgetInfo::selected(egui::WidgetType::Checkbox, enabled, *checked, galley.text())
        });
        if !ui.is_rect_visible(rect) {
            return response;
        }

        let fade = |color: Color32| {
            if enabled {
                color
            } else {
                color.gamma_multiply(0.45)
            }
        };
        let on = *checked || indeterminate;
        let hovered = enabled && response.hovered();
        let square = egui::Rect::from_center_size(
            egui::pos2(rect.left() + BOX / 2.0, rect.center().y),
            Vec2::splat(BOX),
        );
        let (fill, border) = if on {
            let accent = if hovered {
                palette.accent_hover
            } else {
                palette.accent
            };
            (accent, accent)
        } else if hovered {
            (palette.surface, palette.accent)
        } else {
            (palette.surface, palette.secondary.gamma_multiply(0.55))
        };
        let painter = ui.painter();
        if response.has_focus() {
            painter.rect_stroke(
                square.expand(2.0),
                CornerRadius::same(5),
                Stroke::new(2.0, palette.accent.gamma_multiply(0.5)),
                StrokeKind::Outside,
            );
        }
        painter.rect(
            square,
            CornerRadius::same(3),
            fade(fill),
            Stroke::new(1.0, fade(border)),
            StrokeKind::Inside,
        );
        let mark = Stroke::new(1.8, fade(palette.on_accent));
        let at = |x: f32, y: f32| square.min + Vec2::new(x * BOX, y * BOX);
        if *checked {
            painter.line(vec![at(0.25, 0.52), at(0.43, 0.70), at(0.76, 0.33)], mark);
        } else if indeterminate {
            painter.line_segment([at(0.28, 0.5), at(0.72, 0.5)], mark);
        }
        if has_text {
            let color = if enabled { palette.text } else { palette.dim };
            let pos = egui::pos2(
                square.right() + GAP,
                rect.center().y - galley.size().y / 2.0,
            );
            painter.galley(pos, galley, color);
        }
        response
    }
}

/// Adds a [`Check`].
pub fn checkbox(ui: &mut Ui, checked: &mut bool, text: impl Into<egui::WidgetText>) -> Response {
    ui.add(Check::new(checked, text))
}
