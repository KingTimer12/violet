//! An explain's plan drawn as a tree: each step with its cost, rows and
//! time, a bar for its share of the heaviest step, and what to look at.

use egui::{RichText, Sense, Stroke, Ui, Vec2};

use super::theme::{self, Palette};
use super::widgets;
use crate::plan::{Buffers, Plan, PlanNode};

const INDENT: f32 = 18.0;

/// Draws `plan` in the results pane.
pub fn show(ui: &mut Ui, tab: u64, plan: &Plan) {
    let palette = Palette::current(ui.ctx());
    let scale = plan.max_weight();
    egui::ScrollArea::both()
        .id_salt(("plan", tab))
        .auto_shrink(false)
        .show(ui, |ui| {
            ui.add_space(6.0);
            if plan.planning_ms.is_some() || plan.execution_ms.is_some() {
                ui.horizontal(|ui| {
                    ui.add_space(10.0);
                    for (label, ms) in [
                        ("Planning", plan.planning_ms),
                        ("Execution", plan.execution_ms),
                    ] {
                        if let Some(ms) = ms {
                            ui.label(
                                RichText::new(format!("{label} {}", millis(ms)))
                                    .color(palette.secondary),
                            );
                        }
                    }
                });
                ui.add_space(4.0);
            }
            if let Some(buffers) = &plan.planning_buffers {
                ui.horizontal(|ui| {
                    ui.add_space(10.0);
                    ui.label(
                        RichText::new(format!("Planning {}", pages(buffers)))
                            .color(palette.secondary),
                    );
                });
                ui.add_space(4.0);
            }
            if !plan.analyzed() {
                ui.horizontal(|ui| {
                    ui.add_space(10.0);
                    ui.label(
                        RichText::new(
                            "Estimates only: Explain analyze adds actual rows and times.",
                        )
                        .color(palette.dim),
                    );
                });
                ui.add_space(4.0);
            }
            for root in &plan.roots {
                node(ui, &palette, root, 0, scale);
            }
            ui.add_space(8.0);
        });
}

fn node(ui: &mut Ui, palette: &Palette, step: &PlanNode, depth: usize, scale: f64) {
    let share = if scale > 0.0 {
        (step.weight() / scale).clamp(0.0, 1.0) as f32
    } else {
        0.0
    };
    let flagged = !step.warnings.is_empty();
    let frame = egui::Frame::new()
        .inner_margin(egui::Margin::symmetric(8, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add_space(10.0 + INDENT * depth as f32);
                ui.vertical(|ui| {
                    ui.horizontal_wrapped(|ui| {
                        let color = if flagged {
                            palette.warning
                        } else {
                            palette.text
                        };
                        ui.label(
                            RichText::new(&step.label)
                                .font(theme::semibold(13.0))
                                .color(color),
                        );
                        if let Some(target) = &step.target {
                            ui.label(
                                RichText::new(target)
                                    .font(theme::mono(12.0))
                                    .color(palette.accent),
                            );
                        }
                        for warning in &step.warnings {
                            widgets::pill(
                                ui,
                                warning.label(),
                                palette.warning,
                                palette.warning.gamma_multiply(0.15),
                            )
                            .on_hover_text(warning.hint());
                        }
                    });
                    let mut facts = Vec::new();
                    if let Some(cost) = step.cost {
                        facts.push(format!("cost {cost:.2}"));
                    }
                    match (step.est_rows, step.actual_rows) {
                        (Some(est), Some(actual)) => {
                            facts.push(format!("rows {actual:.0} (est {est:.0})"));
                        }
                        (Some(est), None) => facts.push(format!("rows est {est:.0}")),
                        (None, Some(actual)) => facts.push(format!("rows {actual:.0}")),
                        (None, None) => {}
                    }
                    if let Some(ms) = step.time_ms {
                        facts.push(millis(ms));
                    }
                    if let Some(loops) = step.loops.filter(|loops| *loops > 1.0) {
                        facts.push(format!("x{loops:.0} loops"));
                    }
                    if let Some(buffers) = &step.buffers {
                        facts.push(pages(buffers));
                    }
                    if !facts.is_empty() {
                        ui.label(RichText::new(facts.join("  ·  ")).color(palette.secondary));
                    }
                    for detail in &step.details {
                        ui.label(
                            RichText::new(detail)
                                .font(theme::mono(11.0))
                                .color(palette.dim),
                        );
                    }
                    // The step's share of the heaviest one.
                    let (bar, _) = ui.allocate_exact_size(Vec2::new(160.0, 4.0), Sense::hover());
                    ui.painter().rect_filled(bar, 2.0, palette.outline);
                    let mut filled = bar;
                    filled.set_width(bar.width() * share);
                    let fill = if flagged {
                        palette.warning
                    } else {
                        palette.accent
                    };
                    ui.painter().rect_filled(filled, 2.0, fill);
                });
            });
        });
    ui.painter().hline(
        frame.response.rect.x_range(),
        frame.response.rect.bottom(),
        Stroke::new(1.0, palette.outline.gamma_multiply(0.5)),
    );
    for child in &step.children {
        node(ui, palette, child, depth + 1, scale);
    }
}

/// The pages a step touched, and how many came from the cache.
fn pages(buffers: &Buffers) -> String {
    let mut text = format!("pages hit {:.0} read {:.0}", buffers.hit, buffers.read);
    if let Some(ratio) = buffers.hit_ratio() {
        text.push_str(&format!(" ({:.0}% cached)", ratio * 100.0));
    }
    if buffers.written > 0.0 {
        text.push_str(&format!(" written {:.0}", buffers.written));
    }
    if buffers.temp_written > 0.0 || buffers.temp_read > 0.0 {
        text.push_str(&format!(
            " temp {:.0}/{:.0}",
            buffers.temp_read, buffers.temp_written
        ));
    }
    text
}

/// A time in milliseconds, in the unit that reads best.
fn millis(ms: f64) -> String {
    if ms >= 1000.0 {
        format!("{:.2} s", ms / 1000.0)
    } else {
        format!("{ms:.2} ms")
    }
}
