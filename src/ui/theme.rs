//! Violet's look: one palette per theme, mapped onto egui's visuals, and
//! the fonts from fastframe.

use egui::style::{Selection, WidgetVisuals, Widgets};
use egui::{Color32, CornerRadius, FontFamily, FontId, Shadow, Stroke, TextStyle, Theme, Visuals};
use fastframe_fonts::{FontSetup, Weight};

/// Every colour the interface draws with.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Palette {
    pub dark: bool,
    /// Behind everything: the editor and the grid.
    pub window: Color32,
    /// Sidebar, title bar and status bar.
    pub panel: Color32,
    /// Raised things: inputs, cards, menus.
    pub surface: Color32,
    pub surface_hover: Color32,
    pub surface_active: Color32,
    pub outline: Color32,
    pub text: Color32,
    pub secondary: Color32,
    pub dim: Color32,
    pub accent: Color32,
    pub accent_hover: Color32,
    pub accent_soft: Color32,
    pub on_accent: Color32,
    pub success: Color32,
    pub warning: Color32,
    pub danger: Color32,
    pub syntax: Syntax,
}

/// Editor colours.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Syntax {
    pub keyword: Color32,
    pub kind: Color32,
    pub string: Color32,
    pub number: Color32,
    pub comment: Color32,
    pub quoted: Color32,
    pub parameter: Color32,
    pub operator: Color32,
}

const fn hex(rgb: u32) -> Color32 {
    Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

impl Palette {
    pub const DARK: Self = Self {
        dark: true,
        window: hex(0x0E0D13),
        panel: hex(0x14121B),
        surface: hex(0x1B1924),
        surface_hover: hex(0x242130),
        surface_active: hex(0x2D293C),
        outline: hex(0x262331),
        text: hex(0xECEAF4),
        secondary: hex(0xA9A4BA),
        dim: hex(0x6F6982),
        accent: hex(0x8B5CF6),
        accent_hover: hex(0xA78BFA),
        accent_soft: Color32::from_rgba_premultiplied(0x23, 0x17, 0x3F, 0xFF),
        on_accent: hex(0xFFFFFF),
        success: hex(0x34D399),
        warning: hex(0xFBBF24),
        danger: hex(0xF87171),
        syntax: Syntax {
            keyword: hex(0xC4B5FD),
            kind: hex(0x67E8F9),
            string: hex(0x86EFAC),
            number: hex(0xFDBA74),
            comment: hex(0x6F6982),
            quoted: hex(0x93C5FD),
            parameter: hex(0xF9A8D4),
            operator: hex(0xA9A4BA),
        },
    };

    pub const LIGHT: Self = Self {
        dark: false,
        window: hex(0xFFFFFF),
        panel: hex(0xF6F5FA),
        surface: hex(0xFFFFFF),
        surface_hover: hex(0xEFEDF6),
        surface_active: hex(0xE6E2F3),
        outline: hex(0xE3E0EC),
        text: hex(0x1C1A26),
        secondary: hex(0x5A5569),
        dim: hex(0x938DA5),
        accent: hex(0x7C3AED),
        accent_hover: hex(0x6D28D9),
        accent_soft: hex(0xF1EBFE),
        on_accent: hex(0xFFFFFF),
        success: hex(0x059669),
        warning: hex(0xD97706),
        danger: hex(0xDC2626),
        syntax: Syntax {
            keyword: hex(0x7C3AED),
            kind: hex(0x0E7490),
            string: hex(0x15803D),
            number: hex(0xC2410C),
            comment: hex(0x938DA5),
            quoted: hex(0x1D4ED8),
            parameter: hex(0xBE185D),
            operator: hex(0x5A5569),
        },
    };

    #[must_use]
    pub const fn of(theme: Theme) -> Self {
        match theme {
            Theme::Dark => Self::DARK,
            Theme::Light => Self::LIGHT,
        }
    }

    /// The palette for the theme egui is showing now.
    #[must_use]
    pub fn current(ctx: &egui::Context) -> Self {
        Self::of(ctx.theme())
    }

    fn visuals(&self) -> Visuals {
        let mut visuals = if self.dark {
            Visuals::dark()
        } else {
            Visuals::light()
        };
        let radius = CornerRadius::same(6);
        let widget = |fill: Color32, stroke: Color32, text: Color32| WidgetVisuals {
            bg_fill: fill,
            weak_bg_fill: fill,
            bg_stroke: Stroke::new(1.0, stroke),
            corner_radius: radius,
            fg_stroke: Stroke::new(1.0, text),
            expansion: 0.0,
        };
        visuals.widgets = Widgets {
            noninteractive: widget(self.panel, self.outline, self.text),
            inactive: widget(self.surface, self.outline, self.text),
            hovered: widget(self.surface_hover, self.outline, self.text),
            active: widget(self.surface_active, self.accent, self.text),
            open: widget(self.surface_active, self.outline, self.text),
        };
        // Buttons without a frame (`frame(false)`) still read as surfaces.
        visuals.widgets.inactive.weak_bg_fill = self.surface;
        visuals.selection = Selection {
            bg_fill: self
                .accent
                .gamma_multiply(if self.dark { 0.45 } else { 0.25 }),
            stroke: Stroke::new(1.0, self.accent),
        };
        visuals.hyperlink_color = self.accent_hover;
        visuals.faint_bg_color = self.panel;
        visuals.extreme_bg_color = self.window;
        visuals.text_edit_bg_color = Some(self.surface);
        visuals.code_bg_color = self.surface;
        visuals.warn_fg_color = self.warning;
        visuals.error_fg_color = self.danger;
        visuals.window_fill = self.surface;
        visuals.window_stroke = Stroke::new(1.0, self.outline);
        visuals.window_corner_radius = CornerRadius::same(12);
        visuals.menu_corner_radius = CornerRadius::same(8);
        visuals.panel_fill = self.window;
        let shadow = Color32::from_black_alpha(if self.dark { 120 } else { 36 });
        visuals.window_shadow = Shadow {
            offset: [0, 12],
            blur: 32,
            spread: 0,
            color: shadow,
        };
        visuals.popup_shadow = Shadow {
            offset: [0, 6],
            blur: 18,
            spread: 0,
            color: shadow,
        };
        visuals.indent_has_left_vline = false;
        visuals.striped = true;
        visuals.collapsing_header_frame = false;
        visuals.interact_cursor = Some(egui::CursorIcon::PointingHand);
        visuals.text_cursor.stroke = Stroke::new(2.0, self.accent);
        visuals
    }
}

/// Installs fonts, text styles and both themes' visuals. Call once, before
/// the first frame.
pub fn install(ctx: &egui::Context, preference: egui::ThemePreference) {
    let mut fonts = FontSetup::default().definitions();
    let rendering = fastframe_text::detect();
    rendering.apply_to(&mut fonts);
    ctx.set_fonts(fonts);

    for theme in [Theme::Dark, Theme::Light] {
        let palette = Palette::of(theme);
        ctx.style_mut_of(theme, |style| {
            style.visuals = palette.visuals();
            style.text_styles = [
                (
                    TextStyle::Small,
                    FontId::new(11.5, FontFamily::Proportional),
                ),
                (TextStyle::Body, FontId::new(13.5, FontFamily::Proportional)),
                (TextStyle::Button, Weight::Medium.font_id(13.0)),
                (TextStyle::Heading, Weight::SemiBold.font_id(20.0)),
                (
                    TextStyle::Monospace,
                    FontId::new(13.0, FontFamily::Monospace),
                ),
            ]
            .into();
            let spacing = &mut style.spacing;
            spacing.item_spacing = egui::vec2(8.0, 6.0);
            spacing.button_padding = egui::vec2(10.0, 5.0);
            spacing.interact_size.y = 28.0;
            spacing.menu_margin = egui::Margin::same(6);
            spacing.window_margin = egui::Margin::same(20);
            spacing.scroll = egui::style::ScrollStyle::thin();
        });
    }
    // fastframe-text runs after the app's own visuals, for both themes.
    ctx.all_styles_mut(|style| rendering.apply_to_visuals(&mut style.visuals));
    ctx.set_theme(preference);
}

pub fn medium(size: f32) -> FontId {
    Weight::Medium.font_id(size)
}

pub fn semibold(size: f32) -> FontId {
    Weight::SemiBold.font_id(size)
}

pub fn bold(size: f32) -> FontId {
    Weight::Bold.font_id(size)
}

pub fn mono(size: f32) -> FontId {
    FontId::new(size, FontFamily::Monospace)
}

/// A profile's colour tag as a colour.
#[must_use]
pub fn tag(rgb: [u8; 3]) -> Color32 {
    Color32::from_rgb(rgb[0], rgb[1], rgb[2])
}

/// Colour tags offered in the connection form.
pub const TAGS: [[u8; 3]; 7] = [
    [0x8B, 0x5C, 0xF6],
    [0x3B, 0x82, 0xF6],
    [0x10, 0xB9, 0x81],
    [0xF5, 0x9E, 0x0B],
    [0xEF, 0x44, 0x44],
    [0xEC, 0x48, 0x99],
    [0x64, 0x74, 0x8B],
];
