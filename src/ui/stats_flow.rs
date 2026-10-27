//! A table's statistics: size, rows, indexes and their use, last vacuum
//! and analyze, in a dialog opened from the table's menu.

use egui::{CornerRadius, RichText, Sense, Stroke, Vec2};
use violet_app::{CachePolicy, ExecOptions, RequestId};
use violet_core::{ConnectionId, QueryOutput, Result};

use super::VioletApp;
use super::action::Action;
use super::icons::Icon;
use super::state::Loadable;
use super::theme::{self, Palette};
use super::toasts::Toast;
use super::widgets;
use crate::dialect::Family;
use crate::health::bytes;
use crate::stats::TableStats;

/// The open dialog.
pub struct StatsView {
    pub connection: ConnectionId,
    pub schema: String,
    pub table: String,
    pub family: Family,
    pub request: Option<RequestId>,
    pub stats: Loadable<TableStats>,
}

impl VioletApp {
    /// Reads and shows the statistics of `schema.table`.
    pub(super) fn open_table_stats(
        &mut self,
        ctx: &egui::Context,
        connection: ConnectionId,
        schema: String,
        table: String,
    ) {
        let family = self.family(connection);
        let Some(sql) = family.table_stats_query(&schema, &table) else {
            self.toasts.push(
                ctx,
                Toast::info(
                    "Violet does not know this database's catalog, so it has no table statistics",
                ),
            );
            return;
        };
        let options = ExecOptions {
            cache: CachePolicy::Refresh,
            page: None,
        };
        let request = self.backend.execute(connection, sql, options);
        self.table_stats = Some(StatsView {
            connection,
            schema,
            table,
            family,
            request: Some(request),
            stats: Loadable::Loading,
        });
    }

    /// Whether `request` is the dialog's, and takes its answer if so.
    pub(super) fn table_stats_read(
        &mut self,
        request: RequestId,
        result: &Result<QueryOutput>,
    ) -> bool {
        let Some(view) = &mut self.table_stats else {
            return false;
        };
        if view.request != Some(request) {
            return false;
        }
        view.request = None;
        view.stats = match result {
            Ok(output) => match TableStats::from_sets(&output.sets) {
                Some(stats) => Loadable::Ready(stats),
                None => Loadable::Failed("The database reports nothing for this table.".into()),
            },
            Err(error) => Loadable::Failed(error.message.clone()),
        };
        true
    }
}

/// Draws the dialog. Returns `false` once it should close.
pub fn show(ctx: &egui::Context, view: &StatsView, actions: &mut Vec<Action>) -> bool {
    let palette = Palette::current(ctx);
    let mut open = true;
    let modal = egui::Modal::new(egui::Id::new("table-stats"))
        .frame(
            egui::Frame::new()
                .fill(palette.surface)
                .stroke(Stroke::new(1.0, palette.outline))
                .corner_radius(CornerRadius::same(14))
                .shadow(ctx.global_style().visuals.window_shadow)
                .inner_margin(egui::Margin::same(22)),
        )
        .show(ctx, |ui| {
            ui.set_width(560.0);
            ui.spacing_mut().item_spacing.y = 10.0;
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(36.0), Sense::hover());
                ui.painter()
                    .rect_filled(rect, CornerRadius::same(10), palette.accent_soft);
                widgets::paint_icon(ui, Icon::Table, rect.shrink(9.0), palette.accent);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.label(
                        RichText::new(&view.table)
                            .font(theme::semibold(17.0))
                            .color(palette.text),
                    );
                    ui.label(RichText::new(&view.schema).color(palette.secondary));
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                    if widgets::icon_button(ui, Icon::Close, "Close").clicked() {
                        open = false;
                    }
                    if widgets::icon_button(ui, Icon::Refresh, "Read again").clicked() {
                        actions.push(Action::TableStats(
                            view.connection,
                            view.schema.clone(),
                            view.table.clone(),
                        ));
                    }
                });
            });
            match &view.stats {
                Loadable::Idle | Loadable::Loading => {
                    ui.vertical_centered(|ui| ui.add(egui::Spinner::new()));
                }
                Loadable::Failed(message) => {
                    ui.label(RichText::new(message).color(palette.danger));
                }
                Loadable::Ready(stats) => figures(ui, stats, &palette),
            }
            for gap in view.family.stats_gaps() {
                ui.label(RichText::new(*gap).color(palette.dim).size(11.5));
            }
        });
    if modal.should_close() {
        open = false;
    }
    open
}

fn figures(ui: &mut egui::Ui, stats: &TableStats, palette: &Palette) {
    let fact = |ui: &mut egui::Ui, name: &str, value: String| {
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{name}:")).color(palette.secondary));
            ui.label(RichText::new(value).color(palette.text));
        });
    };
    if let Some(total) = stats.total_bytes {
        let mut text = bytes(total);
        if let (Some(table), Some(index)) = (stats.table_bytes, stats.index_bytes) {
            text.push_str(&format!(
                " ({} table, {} indexes)",
                bytes(table),
                bytes(index)
            ));
        }
        fact(ui, "Size", text);
    }
    if let Some(rows) = stats.rows {
        fact(ui, "Rows (estimate)", format!("{rows:.0}"));
    }
    if let Some(share) = stats.dead_share() {
        fact(
            ui,
            "Dead rows",
            format!(
                "{:.0} ({:.1}%)",
                stats.dead_rows.unwrap_or(0.0),
                share * 100.0
            ),
        );
    }
    match (&stats.last_vacuum, &stats.last_analyze) {
        (None, None) => {}
        (vacuum, analyze) => {
            fact(
                ui,
                "Last vacuum",
                vacuum.clone().unwrap_or_else(|| "never".into()),
            );
            fact(
                ui,
                "Last analyze",
                analyze.clone().unwrap_or_else(|| "never".into()),
            );
        }
    }
    if let Some(change) = &stats.last_change {
        fact(ui, "Last change", change.clone());
    }
    if stats.indexes.is_empty() {
        ui.label(RichText::new("No indexes.").color(palette.dim));
        return;
    }
    ui.add_space(4.0);
    ui.label(RichText::new("Indexes").font(theme::semibold(13.0)));
    let idle = stats.unused().len();
    if idle > 0 {
        ui.label(
            RichText::new(format!("{idle} of {} never read", stats.indexes.len()))
                .color(palette.warning),
        );
    }
    for index in &stats.indexes {
        let unused = index.scans == Some(0.0);
        ui.horizontal(|ui| {
            let color = if unused { palette.warning } else { palette.text };
            ui.label(
                RichText::new(&index.name)
                    .font(theme::mono(12.0))
                    .color(color),
            );
            if let Some(scans) = index.scans {
                ui.label(RichText::new(format!("{scans:.0} scans")).color(palette.secondary));
            }
            if let Some(size) = index.bytes {
                ui.label(RichText::new(bytes(size)).color(palette.dim));
            }
            if unused {
                widgets::pill(ui, "Unused", palette.warning, palette.warning.gamma_multiply(0.15))
                    .on_hover_text("Nothing has read this index since the statistics were reset. It slows writes and takes space.");
            }
        });
    }
}
