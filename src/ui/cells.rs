//! How a grid cell shows its value: as text, booleans as check boxes,
//! geometry as well-known text, and foreign keys as links to the row they
//! point at.
//!
//! Cells are painted directly rather than laid out as widgets; the text
//! layout comes from egui's own cache, which knows when the font atlas
//! changes.

use std::borrow::Cow;
use std::sync::Arc;

use egui::text::{LayoutJob, TextWrapping};
use egui::{Color32, CornerRadius, Pos2, Rect, Stroke, TextFormat, Ui, Vec2};
use violet_core::ColumnMeta;

use super::icons::Icon;
use super::theme::{self, Palette};
use super::widgets;

/// Characters shown per cell; the full value is a click away.
pub const CELL_PREVIEW: usize = 300;
/// Room between a cell's edge and its text.
pub const CELL_PADDING: f32 = 6.0;

/// What a cell shows.
#[derive(Clone, Copy)]
pub enum Cell<'a> {
    Value(&'a str),
    Null,
    /// An added row's column with no value: the table's default.
    Default,
    RowNumber(usize),
}

/// How a column's values show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Look {
    Text,
    /// A check box, for the values that read as true or false.
    Boolean,
    /// Well-known binary (PostGIS, MySQL), shown as well-known text.
    Geometry,
}

impl Look {
    pub fn of(column: &ColumnMeta) -> Self {
        let Some(name) = &column.type_name else {
            return Self::Text;
        };
        match name.to_ascii_lowercase().as_str() {
            // MySQL keeps `BOOLEAN` as `tinyint(1)`.
            "bool" | "boolean" | "tinyint(1)" => Self::Boolean,
            "geometry" | "geography" => Self::Geometry,
            _ => Self::Text,
        }
    }
}

/// The value of a boolean cell, as the databases spell it.
pub fn boolean(text: &str) -> Option<bool> {
    match text {
        "t" | "true" | "TRUE" | "1" => Some(true),
        "f" | "false" | "FALSE" | "0" => Some(false),
        _ => None,
    }
}

/// The text a geometry value shows: its well-known text, or the value
/// itself when it does not read as geometry.
pub fn geometry_text(text: &str) -> Cow<'_, str> {
    crate::geometry::wkt(text).map_or(Cow::Borrowed(text), Cow::Owned)
}

/// What [`paint`] drew.
pub struct Painted {
    /// The link's area, when the cell is a foreign key's value.
    pub link: Option<Rect>,
}

/// Paints `cell` in the cell `ui` stands for, the way `look` shows it, cut
/// to its width and centred vertically. With `link`, a value shows as a
/// link to the row it points at.
pub fn paint(
    ui: &Ui,
    cell: Cell<'_>,
    look: Look,
    right_aligned: bool,
    color: Color32,
    link: bool,
) -> Painted {
    let mut painted = Painted { link: None };
    let Cell::Value(text) = cell else {
        paint_text(ui, cell, right_aligned, color);
        return painted;
    };
    match look {
        Look::Boolean => match boolean(text) {
            Some(value) => paint_check(ui, value, color),
            None => paint_text(ui, cell, right_aligned, color),
        },
        Look::Geometry => {
            let shown = cached_geometry(ui, text);
            paint_text(ui, Cell::Value(&shown), false, color);
        }
        _ if link => painted.link = Some(paint_link(ui, text, right_aligned)),
        _ => paint_text(ui, cell, right_aligned, color),
    }
    painted
}

/// The cell's text, laid out to fit its width.
fn galley(ui: &Ui, cell: Cell<'_>, color: Color32, width: f32) -> Arc<egui::Galley> {
    let mut format = TextFormat::simple(theme::mono(12.5), color);
    let text = match cell {
        Cell::Value(text) => preview(text).into_owned(),
        Cell::Null => {
            format.font_id = theme::mono(11.5);
            format.italics = true;
            "NULL".to_owned()
        }
        Cell::Default => {
            format.font_id = theme::mono(11.5);
            format.italics = true;
            "DEFAULT".to_owned()
        }
        Cell::RowNumber(row) => {
            format.font_id = theme::mono(11.0);
            (row + 1).to_string()
        }
    };
    let mut job = LayoutJob::single_section(text, format);
    job.wrap = TextWrapping::truncate_at_width(width.max(0.0));
    ui.painter().layout_job(job)
}

pub fn paint_text(ui: &Ui, cell: Cell<'_>, right_aligned: bool, color: Color32) {
    let rect = ui.max_rect();
    let galley = galley(ui, cell, color, rect.width() - 2.0 * CELL_PADDING);
    let x = if right_aligned {
        rect.right() - CELL_PADDING - galley.size().x
    } else {
        rect.left() + CELL_PADDING
    };
    let y = rect.center().y - galley.size().y / 2.0;
    ui.painter().galley(Pos2::new(x, y), galley, color);
}

/// A check box, ticked for true; `color` dims it with the row.
fn paint_check(ui: &Ui, value: bool, color: Color32) {
    let palette = Palette::current(ui.ctx());
    let rect = ui.max_rect();
    let size = 14.0;
    let left = rect.left() + CELL_PADDING;
    if rect.right() - left < size {
        return;
    }
    let dim = color == palette.dim;
    let accent = if dim {
        palette.dim.gamma_multiply(0.7)
    } else {
        palette.accent
    };
    let check = Rect::from_min_size(
        Pos2::new(left, rect.center().y - size / 2.0),
        Vec2::splat(size),
    );
    if value {
        ui.painter()
            .rect_filled(check, CornerRadius::same(3), accent);
        widgets::paint_icon(ui, Icon::Check, check.shrink(2.0), palette.on_accent);
    } else {
        ui.painter().rect(
            check,
            CornerRadius::same(3),
            Color32::TRANSPARENT,
            Stroke::new(1.0, if dim { palette.dim } else { palette.secondary }),
            egui::StrokeKind::Inside,
        );
    }
}

/// A foreign key's value: accent text on a soft chip with an arrow, which
/// underlines under the pointer. Returns the chip's area, which opens the
/// row it points at.
fn paint_link(ui: &Ui, text: &str, right_aligned: bool) -> Rect {
    let palette = Palette::current(ui.ctx());
    let rect = ui.max_rect();
    let arrow = 11.0;
    let gap = 3.0;
    // The chip's own padding, inside the cell's.
    let pad = 4.0;
    let room = rect.width() - 2.0 * CELL_PADDING - 2.0 * pad - gap - arrow;
    let galley = galley(ui, Cell::Value(text), palette.accent, room);
    let width = galley.size().x + gap + arrow + 2.0 * pad;
    let height = galley.size().y + 4.0;
    let left = if right_aligned {
        rect.right() - CELL_PADDING - width
    } else {
        rect.left() + CELL_PADDING - pad
    }
    .max(rect.left() + 1.0);
    let chip = Rect::from_min_size(
        Pos2::new(left, rect.center().y - height / 2.0),
        Vec2::new(width, height),
    );
    let hovered = ui
        .ctx()
        .pointer_hover_pos()
        .is_some_and(|pointer| chip.contains(pointer) && ui.clip_rect().contains(pointer));
    let fill = if hovered {
        palette.accent.gamma_multiply(0.24)
    } else {
        palette.accent_soft
    };
    ui.painter().rect_filled(chip, CornerRadius::same(4), fill);
    let text_pos = Pos2::new(chip.left() + pad, rect.center().y - galley.size().y / 2.0);
    let text_width = galley.size().x;
    let baseline = text_pos.y + galley.size().y - 1.0;
    ui.painter().galley(text_pos, galley, palette.accent);
    if hovered {
        ui.painter().hline(
            text_pos.x..=text_pos.x + text_width,
            baseline,
            Stroke::new(1.0, palette.accent),
        );
    }
    let icon = Rect::from_min_size(
        Pos2::new(text_pos.x + text_width + gap, rect.center().y - arrow / 2.0),
        Vec2::splat(arrow),
    );
    widgets::paint_icon(ui, Icon::ArrowUpRight, icon, palette.accent);
    chip
}

/// The first line, at most [`CELL_PREVIEW`] characters. Borrowed in the
/// common case, so drawing a cell allocates nothing.
pub fn preview(text: &str) -> Cow<'_, str> {
    let line = text.lines().next().unwrap_or_default();
    let end = line
        .char_indices()
        .nth(CELL_PREVIEW)
        .map_or(line.len(), |(at, _)| at);
    if end == text.len() {
        Cow::Borrowed(text)
    } else {
        Cow::Owned(format!("{}…", &line[..end]))
    }
}

/// Reads geometry once per value while it stays on screen, and keeps only
/// what a cell shows.
#[derive(Default)]
struct Geometry;

impl egui::cache::ComputerMut<&str, Arc<str>> for Geometry {
    fn compute(&mut self, text: &str) -> Arc<str> {
        Arc::from(preview(&geometry_text(text)).as_ref())
    }
}

fn cached_geometry(ui: &Ui, text: &str) -> Arc<str> {
    ui.ctx().memory_mut(|memory| {
        Arc::clone(
            memory
                .caches
                .cache::<egui::cache::FrameCache<Arc<str>, Geometry>>()
                .get(text),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn previews_show_the_first_line() {
        assert_eq!(preview("one\ntwo"), "one…");
        assert_eq!(preview("short"), "short");
        assert!(matches!(preview("short"), Cow::Borrowed(_)));
        let long = "x".repeat(CELL_PREVIEW + 5);
        assert_eq!(preview(&long).chars().count(), CELL_PREVIEW + 1);
    }

    #[test]
    fn looks_follow_the_type() {
        let column = |name: &str| ColumnMeta {
            name: "c".into(),
            type_name: Some(name.into()),
            numeric: false,
        };
        assert_eq!(Look::of(&column("bool")), Look::Boolean);
        assert_eq!(Look::of(&column("BOOLEAN")), Look::Boolean);
        assert_eq!(Look::of(&column("tinyint(1)")), Look::Boolean);
        assert_eq!(Look::of(&column("tinyint")), Look::Text);
        assert_eq!(Look::of(&column("geography")), Look::Geometry);
        assert_eq!(boolean("t"), Some(true));
        assert_eq!(boolean("0"), Some(false));
        assert_eq!(boolean("2"), None);
    }
}
