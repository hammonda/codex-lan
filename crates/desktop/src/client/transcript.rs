//! Transcript output, native-event inspection and pending request controls.

use super::*;

pub(super) fn collapsed_preview(text: &str) -> (&str, bool) {
    let mut lines = 1;
    // Visit at most 1,801 characters and borrow the prefix: large single-line
    // command output must not be scanned or copied every collapsed frame.
    for (count, (offset, character)) in text.char_indices().enumerate() {
        if count == 1800 || (character == '\n' && lines == 6) {
            let more = character != '\n' || offset + character.len_utf8() < text.len();
            return (&text[..offset], more);
        }
        if character == '\n' {
            lines += 1;
        }
    }
    (text, false)
}

pub(super) fn native_record_ui(ui: &mut egui::Ui, index: usize, record: &NativeRecord) {
    let kind = record
        .params
        .pointer("/item/type")
        .and_then(Value::as_str)
        .unwrap_or("");
    let color = if record.method.contains("error") {
        RED
    } else if record.method.contains("reasoning") || kind == "reasoning" {
        LILAC
    } else if record.method.contains("plan") || kind == "userMessage" {
        CYAN
    } else if record.method.contains("agentMessage") || kind == "agentMessage" {
        MINT
    } else if record.method.contains("commandExecution")
        || record.method.contains("fileChange")
        || record.method.contains("diff")
        || matches!(
            kind,
            "commandExecution" | "fileChange" | "mcpToolCall" | "webSearch"
        )
    {
        AMBER
    } else {
        BLUE
    };
    let mut state = egui::collapsing_header::CollapsingState::load_with_default_open(
        ui.ctx(),
        ui.make_persistent_id(("native", index)),
        false,
    );
    let header = ui.horizontal(|ui| {
        if disclosure(ui, state.is_open(), "Expand or collapse this native event").clicked() {
            state.toggle(ui);
        }
        terminal_label(
            ui,
            &format!("{:04} {}", index + 1, record.method),
            ui.available_width() - 26.0,
            11.0,
            color,
        )
        .on_hover_text(&record.method);
        if icon(ui, "⧉", "Copy native event JSON").clicked() {
            ui.ctx().copy_text(pretty(
                &json!({"method":record.method,"params":record.params}),
            ));
        }
    });
    state.show_body_indented(&header.response, ui, |ui| {
        crate::json_view::json_view(ui, ("native_params", index), &record.params);
    });
}

pub(super) fn message_ui(
    ui: &mut egui::Ui,
    message: &mut Message,
    presentation: MessagePresentation,
    native: &[NativeRecord],
) {
    if message.role == "USER" {
        ui.horizontal_top(|ui| {
            ui.label(mono(">", 13.0, CYAN)).on_hover_text("USER");
            let width = (ui.available_width() - 26.0).max(1.0);
            ui.allocate_ui_with_layout(
                egui::vec2(width, 0.0),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.set_min_width(width);
                    ui.set_max_width(width);
                    let job = egui::text::LayoutJob::single_section(
                        message.text.clone(),
                        egui::TextFormat {
                            font_id: egui::FontId::monospace(13.0),
                            line_height: Some(14.0),
                            color: CYAN,
                            ..Default::default()
                        },
                    );
                    ui.add(egui::Label::new(job).selectable(true).wrap());
                },
            );
            if icon(ui, "⧉", "Copy complete output").clicked() {
                ui.ctx().copy_text(message.copy_text(presentation));
            }
        });
        ui.add_space(5.0);
        return;
    }
    let (marker, color) = match message.role {
        "ERROR" => ("!", RED),
        "AGENT" => ("●", MINT),
        "THOUGHT" => ("·", LILAC),
        "DIFF" => ("±", AMBER),
        "PLAN" => ("≡", CYAN),
        "TOOL" => ("$", AMBER),
        _ => ("·", BLUE),
    };
    let is_prose = matches!(message.role, "AGENT" | "ERROR");
    let has_text = message.text.chars().any(|c| !c.is_whitespace());
    ui.horizontal(|ui| {
        // Some native items have metadata but no readable body. Their arrow
        // opens that metadata; it must never just toggle an empty text area.
        let expanded = message.inspecting
            || (has_text
                && if presentation == MessagePresentation::Summary {
                    message.progress_expanded
                } else {
                    message.expanded
                });
        if !is_prose
            && (has_text || message.inspection.is_some())
            && disclosure(
                ui,
                expanded,
                if expanded { "Collapse output" } else { "Expand output" },
            )
            .clicked()
        {
            if presentation == MessagePresentation::Summary {
                message.progress_expanded = !expanded;
            } else {
                message.expanded = !expanded;
            }
            // Collapse closes the whole item, including an open JSON inspector.
            message.inspecting = !expanded && !has_text && message.inspection.is_some();
        }
        ui.label(mono(marker, 12.0, color))
            .on_hover_text(message.role);
        if message.inspection.is_some()
            && ui.add(egui::Button::new(mono("JSON", 10.0, if message.inspecting { CYAN } else { MUTED }))
                .selected(message.inspecting).frame(message.inspecting))
                .on_hover_text(if has_text {
                    "Inspect raw Codex event JSON: metadata and streamed events. The arrow collapses the whole item."
                } else {
                    "Codex supplied no readable text for this item. Open its native events with JSON or the arrow."
                })
                .clicked()
        {
            message.inspecting = !message.inspecting;
        }
        let title = if presentation == MessagePresentation::Summary {
            message.summary_title()
        } else {
            &message.title
        };
        if !title.is_empty() {
            terminal_label(ui, title, ui.available_width() - 26.0, 12.0, color)
                .on_hover_text(title);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if icon(
                ui,
                "⧉",
                if presentation == MessagePresentation::Summary && !message.progress_expanded {
                    "Copy activity heading"
                } else {
                    "Copy complete output"
                },
            )
            .clicked()
            {
                ui.ctx().copy_text(message.copy_text(presentation));
            }
        });
    });
    let show_body = is_prose
        || (has_text
            && if presentation == MessagePresentation::Summary {
                message.progress_expanded
            } else {
                message.expanded
            });
    if show_body {
        message.prepare_json();
    }
    let has_json = message.json.as_ref().is_some_and(Option::is_some);
    let (text, more) = if !show_body {
        ("", false)
    } else if has_json || is_prose || message.full_text {
        (message.text.as_str(), false)
    } else {
        collapsed_preview(&message.text)
    };
    if !text.is_empty() {
        ui.indent("body", |ui| {
            if let Some(Some(value)) = &message.json {
                crate::json_view::json_view(ui, "message_json", value);
            } else {
                let mut job = egui::text::LayoutJob::default();
                for line in text.split_inclusive('\n') {
                    let line_color = match message.role {
                        "AGENT" => INK,
                        "DIFF" if line.starts_with('+') => MINT,
                        "DIFF" if line.starts_with('-') => RED,
                        "DIFF" if line.starts_with("@@") => CYAN,
                        "DIFF" => MUTED,
                        "PLAN" if line.starts_with("[x]") => MINT,
                        "PLAN" if line.starts_with("[ ]") => MUTED,
                        _ => color,
                    };
                    job.append(
                        line,
                        0.0,
                        egui::TextFormat {
                            font_id: egui::FontId::monospace(13.0),
                            line_height: Some(14.0),
                            color: line_color,
                            ..Default::default()
                        },
                    );
                }
                ui.add(egui::Label::new(job).selectable(true).wrap());
            }
        });
    }
    if show_body && !is_prose && !has_json && (more || message.full_text) {
        ui.indent("more_text", |ui| {
            if ui
                .add(egui::Button::new(mono("…", 11.0, MUTED)).frame(false))
                .on_hover_text(if message.full_text {
                    "Show a shorter output preview"
                } else {
                    "Show complete output"
                })
                .clicked()
            {
                message.full_text = !message.full_text;
            }
        });
    }
    if message.inspecting
        && let Some(scope) = &message.inspection
    {
        ui.indent("item_inspection", |ui| {
            ui.label(mono("raw Codex events", 10.0, MUTED));
            let mut found = false;
            for (index, record) in native.iter().enumerate() {
                if scope.matches(index, record) {
                    found = true;
                    native_record_ui(ui, index, record);
                }
            }
            if !found {
                ui.label(mono(
                    "No native events received for this item yet.",
                    11.0,
                    MUTED,
                ));
            }
        });
    }
    ui.add_space(if is_prose { 5.0 } else { 2.0 });
}

pub(super) fn request_ui(ui: &mut egui::Ui, request: &mut Request) -> Option<Value> {
    let mut response = None;
    card(ui, |ui| {
        ui.label(mono("YOUR INPUT IS NEEDED", 11.0, AMBER));
        if let Some(reason) = request.params.get("reason").and_then(Value::as_str) {
            ui.label(reason);
        }
        if request.method == "item/tool/requestUserInput" {
            if let Some(questions) = request.params.get("questions").and_then(Value::as_array) {
                for question in questions {
                    let id = question["id"].as_str().unwrap_or("question").to_owned();
                    ui.label(question["question"].as_str().unwrap_or("Your answer"));
                    let answer = request.answers.entry(id).or_default();
                    if let Some(options) = question.get("options").and_then(Value::as_array) {
                        for option in options {
                            let label = option["label"].as_str().unwrap_or("");
                            if ui
                                .selectable_label(answer == label, label)
                                .on_hover_text(option["description"].as_str().unwrap_or(""))
                                .clicked()
                            {
                                *answer = label.to_owned();
                            }
                        }
                    }
                    ui.add(
                        egui::TextEdit::singleline(answer)
                            .password(question["isSecret"].as_bool().unwrap_or(false))
                            .desired_width(f32::INFINITY)
                            .hint_text("Type an answer"),
                    );
                }
            }
            if ui.button("Send answers").clicked() {
                let answers: serde_json::Map<String, Value> = request
                    .answers
                    .iter()
                    .map(|(id, answer)| (id.clone(), json!({"answers": [answer]})))
                    .collect();
                response = Some(json!({"answers": answers}));
            }
        } else if request.method == "item/commandExecution/requestApproval"
            || request.method == "item/fileChange/requestApproval"
        {
            if let Some(command) = request.params.get("command").and_then(Value::as_str) {
                ui.add(egui::Label::new(command).wrap());
            }
            if let Some(path) = request.params.get("grantRoot").and_then(Value::as_str) {
                ui.label(format!("Requested directory: {path}"));
            }
            ui.horizontal(|ui| {
                if ui.button("Decline").clicked() {
                    response = Some(json!({"decision": "decline"}));
                }
                if ui.button("Allow once").clicked() {
                    response = Some(json!({"decision": "accept"}));
                }
            });
        } else if request.method == "item/permissions/requestApproval" {
            crate::json_view::json_view(
                ui,
                ("permissions", &request.id),
                &request.params["permissions"],
            );
            if ui.button("Keep current permissions").clicked() {
                response = Some(json!({"permissions": {}, "scope": "turn"}));
            }
        } else {
            ui.label("Codex is waiting for a response. You can stop the turn or provide a structured reply below.");
        }
        egui::CollapsingHeader::new("Advanced request details")
            .id_salt(&request.id)
            .show(ui, |ui| {
                ui.label(mono(&request.method, 11.0, MUTED));
                crate::json_view::json_view(ui, ("request_params", &request.id), &request.params);
                ui.add(
                    egui::TextEdit::multiline(&mut request.raw)
                        .desired_width(f32::INFINITY)
                        .desired_rows(3),
                );
                if ui.button("Send JSON reply").clicked() {
                    match serde_json::from_str::<Value>(&request.raw) {
                        Ok(value) if value.is_object() => {
                            response = Some(value);
                            request.error = None;
                        }
                        Ok(_) => request.error = Some("The reply must be a JSON object.".into()),
                        Err(error) => request.error = Some(error.to_string()),
                    }
                }
                if let Some(error) = &request.error {
                    ui.colored_label(RED, error);
                }
            });
    });
    response
}
