use super::*;
use crate::client::transcript::collapsed_preview;

impl SessionObserver {
    pub(crate) fn ui(
        &mut self,
        ui: &mut egui::Ui,
        closed: bool,
    ) -> egui::scroll_area::ScrollAreaOutput<()> {
        ui.spacing_mut().scroll = egui::style::ScrollStyle::solid();
        let mut latest = false;
        let previous_level = self.session.output_level;
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt(("observer_level", &self.session.id))
                .selected_text(self.session.output_level.label()).width(100.0)
                .show_ui(ui, |ui| {
                    for choice in OutputLevel::ALL {
                        ui.selectable_value(&mut self.session.output_level, choice, choice.label()).on_hover_text(choice.description());
                    }
                });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if icon(ui, "⧉", "Copy this transcript in chronological order").clicked() {
                    ui.ctx().copy_text(self.visible_output());
                }
                latest = ui.add(egui::Button::new(mono(if self.at_bottom { "↓ live" } else { "↓ latest" }, 11.0, if self.at_bottom { MINT } else { CYAN })).frame(false))
                    .on_hover_text("Jump to the newest message. Scrolling up pauses automatic following; returning to the bottom resumes it.").clicked();
                let status = if closed { "closed" } else { &self.session.activity };
                let short: String = status.lines().next().unwrap_or("").chars().take(100).collect();
                terminal_label(ui, &short, ui.available_width(), 10.0, MUTED)
                    .on_hover_text("Latest activity. Full event details are available in Everything.");
            });
        });
        if previous_level != self.session.output_level {
            for row in &mut self.rows {
                row.message.inspecting = false;
            }
        }
        ui.separator();
        if self.discarded {
            ui.add(
                egui::Label::new(mono(
                    "Earlier traffic discarded · 8 MiB / 5,000 events",
                    10.0,
                    AMBER,
                ))
                .truncate(),
            );
        }
        let incoming = self.direction(true);
        let outgoing = self.direction(false);
        let level = self.session.output_level;
        let result = egui::ScrollArea::vertical()
            .id_salt(("observer", &self.session.id, level))
            .max_height(ui.available_height())
            .auto_shrink([false, false])
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysVisible)
            .stick_to_bottom(true)
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.set_max_width(ui.available_width());
                ui.push_id((&self.session.id, level), |ui| {
                    if level == OutputLevel::Everything {
                        for (record, stamp) in self.session.native.iter().zip(&self.stamps) {
                            let direction = if stamp.incoming { &incoming } else { &outgoing };
                            ui.push_id(stamp.sequence, |ui| raw_row(ui, *stamp, direction, record));
                        }
                    } else {
                        for row in &mut self.rows {
                            let presentation = level.presentation(&row.message);
                            if presentation != MessagePresentation::Hidden {
                                let direction = if row.stamp.incoming {
                                    &incoming
                                } else {
                                    &outgoing
                                };
                                ui.push_id(row.stamp.sequence, |ui| {
                                    flow_row(ui, row, direction, presentation, &self.session.native)
                                });
                            }
                        }
                    }
                    if self.rows.is_empty() && self.session.native.is_empty() {
                        ui.label(mono("Waiting for session traffic…", 12.0, MUTED));
                    }
                    if latest {
                        ui.scroll_to_cursor(Some(egui::Align::BOTTOM));
                    }
                });
            });
        self.at_bottom =
            result.content_size.y - result.inner_rect.height() - result.state.clamped_offset().y
                <= 3.0;
        result
    }
}

fn stamp_label(ui: &mut egui::Ui, stamp: Stamp) {
    let seconds = stamp.elapsed.as_secs();
    ui.label(mono(format!("{:03} {:02}:{:02}", stamp.sequence, seconds / 60, seconds % 60), 10.0, MUTED))
        .on_hover_text("Server arrival sequence and elapsed session time. Hidden events explain gaps in sequence numbers.");
}

fn flow_row(
    ui: &mut egui::Ui,
    row: &mut FlowRow,
    direction: &str,
    presentation: MessagePresentation,
    native: &[NativeRecord],
) {
    let message = &mut row.message;
    let direction_color = if row.stamp.incoming { CYAN } else { MINT };
    let kind_color = match message.role {
        "ERROR" => RED,
        "THOUGHT" => LILAC,
        "TOOL" | "DIFF" => AMBER,
        _ => MUTED,
    };
    let has_body = !message.text.trim().is_empty();
    let expanded = if presentation == MessagePresentation::Summary {
        &mut message.progress_expanded
    } else {
        &mut message.expanded
    };
    ui.horizontal(|ui| {
        let open = (has_body && *expanded) || message.inspecting;
        if disclosure(
            ui,
            open,
            if open {
                "Collapse message"
            } else {
                "Expand message"
            },
        )
        .clicked()
        {
            *expanded = !open && has_body;
            message.inspecting = !open && !has_body;
        }
        stamp_label(ui, row.stamp);
        ui.label(mono(direction, 11.0, direction_color));
        terminal_label(
            ui,
            &message.title,
            (ui.available_width() - 74.0).max(1.0),
            11.0,
            kind_color,
        )
        .on_hover_text(&message.title);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if icon(ui, "⧉", "Copy complete message").clicked() {
                ui.ctx().copy_text(if has_body {
                    message.text.clone()
                } else {
                    message.title.clone()
                });
            }
            if ui
                .add(
                    egui::Button::new(mono(
                        "JSON",
                        10.0,
                        if message.inspecting { CYAN } else { MUTED },
                    ))
                    .frame(message.inspecting),
                )
                .on_hover_text("Inspect the received events for this message")
                .clicked()
            {
                message.inspecting = !message.inspecting;
            }
        });
    });
    if *expanded && has_body {
        ui.indent("body", |ui| {
            message.prepare_json();
            if let Some(Some(value)) = &message.json {
                crate::json_view::json_view(ui, "message_json", value);
            } else {
                let (text, more) = if message.full_text {
                    (message.text.as_str(), false)
                } else {
                    collapsed_preview(&message.text)
                };
                let job = egui::text::LayoutJob::single_section(
                    text.to_owned(),
                    egui::TextFormat {
                        font_id: egui::FontId::monospace(13.0),
                        line_height: Some(15.0),
                        color: if row.stamp.incoming {
                            CYAN
                        } else if message.role == "ERROR" {
                            RED
                        } else {
                            INK
                        },
                        ..Default::default()
                    },
                );
                ui.add(egui::Label::new(job).selectable(true).wrap());
                if (more || message.full_text)
                    && ui
                        .add(
                            egui::Button::new(mono(
                                if message.full_text {
                                    "… less"
                                } else {
                                    "… more"
                                },
                                10.0,
                                MUTED,
                            ))
                            .frame(false),
                        )
                        .clicked()
                {
                    message.full_text = !message.full_text;
                }
            }
        });
    }
    if message.inspecting {
        ui.indent("received_events", |ui| {
            for &index in &row.records {
                crate::client::transcript::native_record_ui(ui, index, &native[index]);
            }
        });
    }
    ui.add_space(5.0);
}

fn raw_row(ui: &mut egui::Ui, stamp: Stamp, direction: &str, record: &NativeRecord) {
    let mut state = egui::collapsing_header::CollapsingState::load_with_default_open(
        ui.ctx(),
        ui.make_persistent_id("raw"),
        false,
    );
    let header = ui.horizontal(|ui| {
        if disclosure(ui, state.is_open(), "Expand or collapse event JSON").clicked() { state.toggle(ui); }
        stamp_label(ui, stamp);
        ui.label(mono(direction, 11.0, if stamp.incoming { CYAN } else { MINT }));
        terminal_label(ui, &record.method, (ui.available_width() - 26.0).max(1.0), 11.0, MUTED).on_hover_text(&record.method);
        if icon(ui, "⧉", "Copy event JSON").clicked() {
            ui.ctx().copy_text(pretty(&json!({"sequence":stamp.sequence,"direction":direction,"method":record.method,"params":record.params})));
        }
    });
    state.show_body_indented(&header.response, ui, |ui| {
        crate::json_view::json_view(ui, "params", &record.params)
    });
}
