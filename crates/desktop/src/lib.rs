mod allowance_ui;
mod client;
mod json_view;
mod prompt;
mod server;
mod usage;

use eframe::egui::{self, Color32, FontFamily, FontId, RichText, Stroke};
use std::{path::PathBuf, time::Duration};
use tokio::{runtime::Runtime, sync::oneshot};
use tokio_util::sync::CancellationToken;

pub(crate) const BG: Color32 = Color32::from_rgb(10, 15, 22);
pub(crate) const PANEL: Color32 = Color32::from_rgb(15, 22, 31);
pub(crate) const BORDER: Color32 = Color32::from_rgb(37, 52, 67);
pub(crate) const INK: Color32 = Color32::from_rgb(213, 225, 236);
pub(crate) const MUTED: Color32 = Color32::from_rgb(119, 143, 163);
pub(crate) const CYAN: Color32 = Color32::from_rgb(87, 209, 230);
pub(crate) const MINT: Color32 = Color32::from_rgb(125, 224, 175);
pub(crate) const AMBER: Color32 = Color32::from_rgb(242, 192, 114);
pub(crate) const RED: Color32 = Color32::from_rgb(246, 132, 132);
pub(crate) const LILAC: Color32 = Color32::from_rgb(186, 162, 235);
pub(crate) const BLUE: Color32 = Color32::from_rgb(132, 181, 246);

pub(crate) fn theme(ctx: &egui::Context) {
    let mut style = egui::Style {
        visuals: egui::Visuals::dark(),
        override_font_id: Some(FontId::new(13.0, FontFamily::Monospace)),
        ..Default::default()
    };
    style.visuals.override_text_color = Some(INK);
    style.visuals.panel_fill = BG;
    style.visuals.window_fill = PANEL;
    style.visuals.extreme_bg_color = BG;
    style.visuals.faint_bg_color = PANEL;
    style.visuals.selection.bg_fill = Color32::from_rgb(28, 80, 91);
    style.visuals.selection.stroke = Stroke::new(1.0, CYAN);
    style.visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
    style.visuals.widgets.inactive.weak_bg_fill = Color32::from_rgb(22, 34, 46);
    style.visuals.widgets.inactive.bg_fill = PANEL;
    style.visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, BORDER);
    style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(30, 52, 65);
    style.visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, CYAN);
    style.visuals.widgets.active.bg_fill = Color32::from_rgb(36, 68, 76);
    style.spacing.item_spacing = egui::vec2(6.0, 2.0);
    style.spacing.button_padding = egui::vec2(6.0, 2.0);
    style.spacing.icon_width = 12.0;
    style.spacing.icon_width_inner = 8.0;
    ctx.set_theme(egui::Theme::Dark);
    ctx.set_style_of(egui::Theme::Dark, style);
}

pub(crate) fn mono(text: impl Into<String>, size: f32, color: Color32) -> RichText {
    RichText::new(text)
        .font(FontId::monospace(size))
        .color(color)
}

pub(crate) fn panel_frame() -> egui::Frame {
    egui::Frame::new().fill(BG).inner_margin(8)
}

pub(crate) fn card(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new()
        .fill(PANEL)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(2)
        .inner_margin(8)
        .show(ui, add);
}

pub(crate) fn icon(ui: &mut egui::Ui, symbol: &str, tooltip: &str) -> egui::Response {
    icon_with_hover_color(ui, symbol, tooltip, INK)
}

pub(crate) fn icon_with_hover_color(
    ui: &mut egui::Ui,
    symbol: &str,
    tooltip: &str,
    hover_color: Color32,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(20.0, 20.0), egui::Sense::click());
    response
        .widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, ui.is_enabled(), tooltip));
    let response = response.on_hover_text(tooltip);
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        let highlighted = ui.is_enabled() && (response.hovered() || response.has_focus());
        let color = if !ui.is_enabled() {
            MUTED.gamma_multiply(0.4)
        } else if highlighted {
            hover_color
        } else {
            MUTED
        };
        if highlighted {
            let fill = if hover_color == INK {
                PANEL
            } else {
                hover_color.gamma_multiply(0.15)
            };
            painter.rect_filled(rect.shrink(1.0), 2, fill);
        }
        let stroke = Stroke::new(1.2, color);
        let origin = rect.center() - egui::vec2(6.0, 6.0);
        let p = |x: f32, y: f32| origin + egui::vec2(x, y);
        let line = |a, b| {
            painter.line_segment([a, b], stroke);
        };
        match symbol {
            "☰" => {
                for y in [2.0, 6.0, 10.0] {
                    line(p(1.0, y), p(11.0, y));
                }
            }
            "⧉" => {
                painter.rect_stroke(
                    egui::Rect::from_min_max(p(1.0, 1.0), p(9.0, 9.0)),
                    1,
                    stroke,
                    egui::StrokeKind::Inside,
                );
                let front = egui::Rect::from_min_max(p(4.0, 4.0), p(12.0, 12.0));
                painter.rect_filled(front, 1, BG);
                painter.rect_stroke(front, 1, stroke, egui::StrokeKind::Inside);
            }
            "◉" => {
                painter.add(egui::Shape::closed_line(
                    vec![
                        p(0.0, 6.0),
                        p(3.0, 3.0),
                        p(6.0, 2.0),
                        p(9.0, 3.0),
                        p(12.0, 6.0),
                        p(9.0, 9.0),
                        p(6.0, 10.0),
                        p(3.0, 9.0),
                    ],
                    stroke,
                ));
                painter.circle_stroke(p(6.0, 6.0), 2.0, stroke);
            }
            "↻" | "⏻" => {
                let points = (0..=24)
                    .map(|i| {
                        let angle = -std::f32::consts::FRAC_PI_4
                            + i as f32 * std::f32::consts::PI * 1.5 / 24.0;
                        p(6.0 + 5.0 * angle.cos(), 6.0 + 5.0 * angle.sin())
                    })
                    .collect();
                painter.add(egui::Shape::line(points, stroke));
                if symbol == "⏻" {
                    line(p(6.0, 0.0), p(6.0, 6.0));
                } else {
                    line(p(10.0, 0.0), p(10.0, 4.0));
                    line(p(6.0, 4.0), p(10.0, 4.0));
                }
            }
            "+" => {
                line(p(6.0, 1.0), p(6.0, 11.0));
                line(p(1.0, 6.0), p(11.0, 6.0));
            }
            "×" => {
                line(p(2.0, 2.0), p(10.0, 10.0));
                line(p(2.0, 10.0), p(10.0, 2.0));
            }
            "↑" => {
                line(p(6.0, 11.0), p(6.0, 1.0));
                line(p(2.0, 5.0), p(6.0, 1.0));
                line(p(10.0, 5.0), p(6.0, 1.0));
            }
            "■" => {
                painter.rect_filled(
                    egui::Rect::from_min_max(p(2.0, 2.0), p(10.0, 10.0)),
                    1,
                    color,
                );
            }
            "▶" | "▸" => {
                painter.add(egui::Shape::convex_polygon(
                    vec![p(3.0, 1.0), p(11.0, 6.0), p(3.0, 11.0)],
                    color,
                    Stroke::NONE,
                ));
            }
            "▾" => {
                painter.add(egui::Shape::convex_polygon(
                    vec![p(1.0, 3.0), p(11.0, 3.0), p(6.0, 11.0)],
                    color,
                    Stroke::NONE,
                ));
            }
            _ => {
                painter.circle_stroke(p(6.0, 6.0), 4.0, stroke);
            }
        }
    }
    response
}

/// One disclosure icon for transcript bodies, native events and JSON levels.
pub(crate) fn disclosure(ui: &mut egui::Ui, expanded: bool, tooltip: &str) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(16.0, 18.0), egui::Sense::click());
    response
        .widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, ui.is_enabled(), tooltip));
    if ui.is_rect_visible(rect) {
        let color = if response.hovered() || response.has_focus() {
            INK
        } else {
            MUTED
        };
        let center = rect.center();
        let points = if expanded {
            vec![
                center + egui::vec2(-4.0, -2.0),
                center + egui::vec2(4.0, -2.0),
                center + egui::vec2(0.0, 3.0),
            ]
        } else {
            vec![
                center + egui::vec2(-2.0, -4.0),
                center + egui::vec2(3.0, 0.0),
                center + egui::vec2(-2.0, 4.0),
            ]
        };
        ui.painter()
            .add(egui::Shape::convex_polygon(points, color, Stroke::NONE));
    }
    response.on_hover_text(tooltip)
}

pub(crate) fn terminal_label(
    ui: &mut egui::Ui,
    text: &str,
    width: f32,
    size: f32,
    color: Color32,
) -> egui::Response {
    let width = width.max(0.0);
    ui.allocate_ui_with_layout(
        egui::vec2(width, 18.0),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.set_min_width(width);
            ui.add(egui::Label::new(mono(text, size, color)).truncate())
        },
    )
    .inner
}

pub(crate) fn field(ui: &mut egui::Ui, label: &str, text: &mut String, hint: &str) {
    ui.label(mono(label, 11.0, MUTED));
    ui.add(
        egui::TextEdit::singleline(text)
            .desired_width(f32::INFINITY)
            .hint_text(hint),
    );
}

pub(crate) fn workspace_path(ui: &mut egui::Ui, path: &str) {
    ui.horizontal(|ui| {
        terminal_label(ui, path, ui.available_width() - 26.0, 11.0, MUTED).on_hover_text(path);
        if icon(ui, "⧉", "Copy server directory path").clicked() {
            ui.ctx().copy_text(path.to_owned());
        }
    });
}

pub(crate) fn state_directory() -> PathBuf {
    if cfg!(target_os = "windows") {
        if let Some(path) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(path).join("codex-lan");
        }
    } else {
        if let Some(path) = std::env::var_os("XDG_STATE_HOME") {
            return PathBuf::from(path).join("codex-lan");
        }
        if let Some(path) = std::env::var_os("HOME") {
            return PathBuf::from(path).join(".local/state/codex-lan");
        }
    }
    PathBuf::from(".codex-lan")
}

pub(crate) struct Worker {
    pub stop: CancellationToken,
    pub done: oneshot::Receiver<Result<(), String>>,
}

pub(crate) fn runtime() -> Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("Cannot start networking runtime")
}

pub(crate) fn closing(ctx: &egui::Context, close_requested: &mut bool, worker: &Option<Worker>) {
    if ctx.input(|i| i.viewport().close_requested()) {
        *close_requested = true;
    }
    if *close_requested {
        if let Some(worker) = worker {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            worker.stop.cancel();
        } else {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
    ctx.request_repaint_after(Duration::from_millis(33));
}

pub fn run_client(demo: bool) -> eframe::Result {
    eframe::run_native(
        "Codex LAN / Client",
        options(),
        Box::new(move |cc| {
            theme(&cc.egui_ctx);
            Ok(Box::new(client::ClientApp::new(demo)))
        }),
    )
}

pub fn run_server(demo: bool) -> eframe::Result {
    eframe::run_native(
        "Codex LAN / Server",
        options(),
        Box::new(move |cc| {
            theme(&cc.egui_ctx);
            Ok(Box::new(server::ServerApp::new(demo)))
        }),
    )
}

fn options() -> eframe::NativeOptions {
    eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 800.0])
            .with_min_inner_size([860.0, 620.0]),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_window_close_cancels_networking_and_waits_for_worker_shutdown() {
        let ctx = egui::Context::default();
        let (done_sender, done) = oneshot::channel();
        let stop = CancellationToken::new();
        let worker = Some(Worker {
            stop: stop.clone(),
            done,
        });
        let mut close_requested = false;
        let mut input = egui::RawInput::default();
        input
            .viewports
            .get_mut(&egui::ViewportId::ROOT)
            .unwrap()
            .events
            .push(egui::ViewportEvent::Close);
        let mut output = ctx.run_ui(input, |ui| closing(ui.ctx(), &mut close_requested, &worker));
        assert!(close_requested);
        assert!(stop.is_cancelled());
        let commands = &output.viewport_output[&egui::ViewportId::ROOT].commands;
        assert!(
            commands
                .iter()
                .any(|c| matches!(c, egui::ViewportCommand::CancelClose))
        );
        assert!(
            !commands
                .iter()
                .any(|c| matches!(c, egui::ViewportCommand::Close))
        );
        output.textures_delta.clear();
        drop(worker);
        drop(done_sender);
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            closing(ui.ctx(), &mut close_requested, &None)
        });
        assert!(
            output.viewport_output[&egui::ViewportId::ROOT]
                .commands
                .iter()
                .any(|c| matches!(c, egui::ViewportCommand::Close))
        );
        output.textures_delta.clear();
    }
}
