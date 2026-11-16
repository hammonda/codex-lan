//! Client window layout and controls.

use super::transcript::{message_ui, native_record_ui, request_ui};
use super::*;

impl ClientApp {
    pub(super) fn sidebar(&mut self, ui: &mut egui::Ui) {
        let mut close = None;
        egui::Panel::left("session_sidebar")
            .default_size(230.0)
            .size_range(190.0..=380.0)
            .frame(panel_frame())
            .show(ui, |ui| {
                if !self.connected {
                    ui.add_enabled_ui(self.worker.is_none() && !self.demo, |ui| {
                        field(ui, "server", &mut self.address, "192.168.1.42:7331");
                        ui.horizontal(|ui| {
                            ui.label(mono("pairing", 11.0, MUTED));
                            if icon(ui, "◉", "Show or hide pairing code").clicked() {
                                self.reveal_pairing = !self.reveal_pairing;
                            }
                        });
                        ui.add(
                            egui::TextEdit::singleline(&mut self.pairing)
                                .password(!self.reveal_pairing)
                                .desired_width(f32::INFINITY)
                                .hint_text("Paste pairing code"),
                        );
                        if ui
                            .add_enabled(
                                !self.address.trim().is_empty() && !self.pairing.trim().is_empty(),
                                egui::Button::new("connect →"),
                            )
                            .clicked()
                        {
                            self.connect();
                        }
                    });
                    if self.worker.is_some() {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            if icon(ui, "×", "Cancel connection").clicked()
                                && let Some(worker) = &self.worker
                            {
                                worker.stop.cancel();
                                self.stopping = true;
                            }
                        });
                    }
                    ui.separator();
                }
                ui.horizontal(|ui| {
                    if ui.add(egui::Button::new(mono("+", 14.0, CYAN))
                        .min_size(egui::vec2(24.0, 24.0))
                        .fill(PANEL).stroke(Stroke::new(1.0, BORDER)).corner_radius(5))
                        .on_hover_text("New session: choose model, effort and server folder").clicked() {
                        self.show_new = !self.show_new;
                    }
                    ui.label(mono(
                        format!("sessions / {}", self.sessions.len()),
                        11.0,
                        MUTED,
                    ));
                });
                if self.show_new {
                    self.new_session_form(ui);
                    ui.separator();
                }
                if self.new_pending {
                    ui.label(mono("creating…", 11.0, AMBER));
                }
                egui::ScrollArea::vertical()
                    .id_salt("sessions_scroll")
                    .show(ui, |ui| {
                        for session in &self.sessions {
                            let selected = self.selected.as_ref() == Some(&session.id);
                            let marker = if session.closing {
                                "×"
                            } else if session.active {
                                "●"
                            } else {
                                "·"
                            };
                            ui.push_id(&session.id, |ui| {
                                egui::Frame::new()
                                    .fill(if selected { ui.visuals().selection.bg_fill } else { PANEL })
                                    .stroke(Stroke::new(1.0, if selected { CYAN.gamma_multiply(0.35) } else { BORDER }))
                                    .corner_radius(4).inner_margin(2).show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    let width = (ui.available_width() - 20.0 - ui.spacing().item_spacing.x).max(0.0);
                                    if ui.add_sized([width, 20.0], egui::Button::new(mono(
                                        format!("{marker} {}", session.name), 12.0,
                                        if session.closing { AMBER } else if session.active { MINT } else if selected { CYAN } else { MUTED },
                                    )).truncate().frame(false))
                                        .on_hover_text(&session.workspace).clicked() {
                                        self.selected = Some(session.id.clone());
                                    }
                                    ui.add_enabled_ui(self.connected && !self.demo && !self.stopping && !session.closing, |ui| {
                                        if icon_with_hover_color(ui, "×", &format!("Close {} and delete its native Codex thread. Files remain; server stays running.", session.name), RED).clicked() {
                                            close = Some(session.id.clone());
                                        }
                                    });
                                });
                                });
                            });
                        }
                    });
            });
        if let Some(session_id) = close {
            self.close_session(&session_id);
        }
    }

    fn new_session_form(&mut self, ui: &mut egui::Ui) {
        let mut create = false;
        ui.add_enabled_ui(self.connected && !self.stopping && !self.new_pending && self.sessions.len() < 8 && !self.demo, |ui| {
            ui.add(egui::TextEdit::singleline(&mut self.new_name).desired_width(f32::INFINITY).hint_text("Session name"));
            let previous_model = self.new_model.clone();
            egui::ComboBox::from_id_salt("new_model").width(ui.available_width()-10.0).selected_text(self.new_model.as_deref().unwrap_or("Server default model")).show_ui(ui, |ui| {
                ui.selectable_value(&mut self.new_model, None, "Server default model");
                for model in &self.models { ui.selectable_value(&mut self.new_model, Some(model.model.clone()), &model.display_name); }
            });
            if previous_model != self.new_model { self.new_effort = None; }
            let model = self.new_model.as_ref().and_then(|selected| self.models.iter().find(|m| &m.model == selected));
            ui.add_enabled_ui(model.is_some(), |ui| {
                egui::ComboBox::from_id_salt("new_effort").width(ui.available_width()-10.0).selected_text(self.new_effort.as_deref().unwrap_or("Default effort")).show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.new_effort, None, "Default effort");
                    if let Some(model) = model {
                        for effort in &model.efforts { ui.selectable_value(&mut self.new_effort, Some(effort.clone()), effort); }
                    }
                }).response.on_hover_text("Choose a model to select its supported effort. With no model selected, both inherit the server's Codex configuration.");
            });
            ui.add(egui::TextEdit::singleline(&mut self.new_workspace).desired_width(f32::INFINITY).hint_text("Folder · automatic if blank")).on_hover_text(format!("Optional server directory inside {}. Relative paths start at this root. An existing directory can be used. Blank creates a unique folder under codex-lan-threads/ inside this root. Files remain after session closure.", self.workspace));
            ui.horizontal(|ui| {
                create = ui.button("create →").clicked();
                if icon(ui, "↻", "Refresh available models from Codex").clicked() { self.send(ClientCommand::ListModels); }
            });
        });
        if create {
            let name = if self.new_name.trim().is_empty() {
                format!("Session {:02}", self.sessions.len() + 1)
            } else {
                self.new_name.trim().into()
            };
            let options = SessionOptions {
                model: self.new_model.clone(),
                effort: self.new_effort.clone(),
                workspace: (!self.new_workspace.trim().is_empty())
                    .then(|| self.new_workspace.trim().into()),
                ..Default::default()
            };
            if self.send(ClientCommand::NewSession { name, options }) {
                self.new_pending = true;
                self.new_name.clear();
            }
        }
    }

    pub(super) fn conversation(&mut self, ui: &mut egui::Ui) {
        let Some(index) = self
            .sessions
            .iter()
            .position(|s| Some(&s.id) == self.selected.as_ref())
        else {
            egui::CentralPanel::default()
                .frame(panel_frame())
                .show(ui, |ui| {
                    ui.label(mono("› codex lan", 15.0, INK));
                    ui.label(mono(
                        if self.connected {
                            "Create a session with + to begin."
                        } else {
                            "Connect to your server to begin."
                        },
                        12.0,
                        MUTED,
                    ));
                });
            return;
        };
        let session = &mut self.sessions[index];
        let mut command = None;
        egui::Panel::top("session_header").frame(panel_frame()).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(mono(&session.name, 13.0, INK));
                if let Some(model) = &session.model { ui.label(mono(model, 11.0, MUTED)); }
                if let Some(effort) = &session.effort { ui.label(mono(effort, 11.0, MUTED)); }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let mut selected_level = session.output_level;
                    egui::ComboBox::from_id_salt(("output_level", &session.id))
                        .width(104.0)
                        .selected_text(session.output_level.label())
                        .show_ui(ui, |ui| {
                            for level in OutputLevel::ALL {
                                ui.selectable_value(&mut selected_level, level, level.label())
                                    .on_hover_text(level.description());
                            }
                        }).response.on_hover_text("Output detail. Switch at any time; all received data stays available. Use JSON beside an item to inspect its raw native events.");
                    session.set_output_level(selected_level);
                    if icon(ui, "⧉", if session.output_level == OutputLevel::Everything { "Copy all native events and local input/errors as JSONL" } else { "Copy this output level; includes full text of visible output, with Progress tool bodies only when expanded" }).clicked() {
                        ui.ctx().copy_text(session.visible_output());
                    }
                });
            });
            workspace_path(ui, &session.workspace);
        });
        egui::Panel::bottom("composer")
            .frame(panel_frame())
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if session.active {
                        ui.spinner();
                    }
                    let status = if session.closing {
                        "deleting native thread…"
                    } else if session.output_level == OutputLevel::Replies {
                        if session.active { "working" } else { "ready" }
                    } else {
                        &session.activity
                    };
                    ui.add(egui::Label::new(mono(status, 10.0, MUTED)).truncate());
                    if session.pending_prompts > 0 {
                        ui.label(mono(
                            format!("{} pending", session.pending_prompts),
                            10.0,
                            AMBER,
                        ));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(mono(
                            session.usage.label(session.model.as_deref()),
                            10.0,
                            AMBER,
                        ))
                        .on_hover_text(session.usage.tooltip(session.model.as_deref()));
                    });
                });
                let enabled = !self.demo && !session.closing && !self.stopping;
                let mut send = false;
                ui.add_enabled_ui(enabled, |ui| {
                    ui.horizontal_top(|ui| {
                        ui.label(mono(">", 13.0, CYAN));
                        let response = ui
                            .allocate_ui_with_layout(
                                egui::vec2((ui.available_width() - 44.0).max(0.0), 30.0),
                                egui::Layout::top_down(egui::Align::Min),
                                |ui| {
                                    crate::prompt::prompt_input(
                                        ui,
                                        &mut session.draft,
                                        ("prompt", &session.id),
                                        self.demo,
                                    )
                                    .on_hover_text(
                                        if session.active {
                                            "Steer Codex · Enter to send · Shift+Enter for a new line"
                                        } else {
                                            "Prompt · Enter to send · Shift+Enter for a new line"
                                        },
                                    )
                                },
                            )
                            .inner;
                        send = response.has_focus()
                            && ui.input(|i| i.events.iter().any(|event| matches!(event,
                                egui::Event::Key {
                                    key: egui::Key::Enter,
                                    pressed: true,
                                    repeat: false,
                                    modifiers,
                                    ..
                                } if !modifiers.shift && !modifiers.alt
                            )));
                        ui.vertical(|ui| {
                            ui.add_enabled_ui(!session.draft.trim().is_empty(), |ui| {
                                send |= icon(
                                    ui,
                                    "↑",
                                    if session.active {
                                        "Send steering prompt · Enter"
                                    } else {
                                        "Send prompt · Enter"
                                    },
                                )
                                .clicked();
                            });
                            if session.active && icon(ui, "■", "Interrupt current turn").clicked()
                            {
                                command = Some(ClientCommand::Interrupt {
                                    session_id: session.id.clone(),
                                });
                            }
                        });
                    });
                });
                if send && enabled && !session.draft.trim().is_empty() {
                    let text = session.draft.trim().to_owned();
                    session.messages.push(Message::text("USER", text.clone()));
                    session.draft.clear();
                    session.pending_prompts += 1;
                    command = Some(ClientCommand::Prompt {
                        session_id: session.id.clone(),
                        text,
                    });
                }
            });
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(BG).inner_margin(12))
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt(("transcript", &session.id, session.output_level))
                    .stick_to_bottom(true)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.push_id((&session.id, session.output_level), |ui| {
                            ui.set_width(ui.available_width());
                            if session.output_level == OutputLevel::Everything {
                                for (index, message) in
                                    session.messages.iter_mut().enumerate().filter(
                                        |(_, message)| matches!(message.role, "USER" | "ERROR"),
                                    )
                                {
                                    ui.push_id(("local", index), |ui| {
                                        message_ui(
                                            ui,
                                            message,
                                            MessagePresentation::Full,
                                            &session.native,
                                        );
                                    });
                                }
                                ui.label(mono(
                                    format!("native events / {}", session.native.len()),
                                    10.0,
                                    MUTED,
                                ));
                                for (index, record) in session.native.iter().enumerate() {
                                    native_record_ui(ui, index, record);
                                }
                            } else {
                                for (index, message) in session.messages.iter_mut().enumerate() {
                                    let presentation = session.output_level.presentation(message);
                                    if presentation != MessagePresentation::Hidden {
                                        ui.push_id(index, |ui| {
                                            message_ui(ui, message, presentation, &session.native)
                                        });
                                    }
                                }
                                if session.messages.is_empty() {
                                    ui.label(mono(
                                        "Ready. Prompts can also steer a running turn.",
                                        12.0,
                                        MUTED,
                                    ));
                                }
                            }
                            for request in &mut session.requests {
                                if let Some(result) = request_ui(ui, request) {
                                    command = Some(ClientCommand::Reply {
                                        session_id: session.id.clone(),
                                        request_id: request.id.clone(),
                                        result,
                                    });
                                }
                            }
                        });
                    });
            });
        if let Some(command) = command {
            self.send(command);
        }
    }
}

impl eframe::App for ClientApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain();
        closing(ctx, &mut self.close_requested, &self.worker);
    }
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let (status, color) = if self.demo {
            ("demo", AMBER)
        } else if self.stopping || self.close_requested {
            ("disconnecting", AMBER)
        } else if self.connected {
            ("connected", MINT)
        } else if self.worker.is_some() {
            ("connecting", CYAN)
        } else {
            ("offline", MUTED)
        };
        egui::Panel::top("app_header").frame(panel_frame()).show(ui,|ui| {
            ui.horizontal(|ui| {
                if icon(ui,"☰","Show or hide sessions and connection settings").clicked(){self.show_sidebar= !self.show_sidebar;}
                ui.label(mono("codex / lan",13.0,INK)).on_hover_text(format!("v{}",env!("CARGO_PKG_VERSION")));
                ui.label(mono(format!("● {status}"),10.0,color)).on_hover_text("Encrypted LAN connection. Session closure deletes its native thread; files remain on server.");
                if self.connected {terminal_label(ui, &self.address, (ui.available_width()-210.0).clamp(40.0,220.0), 11.0, MUTED).on_hover_text(&self.workspace);}
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center),|ui| {
                    if self.connected && icon(ui,"⏻","Disconnect and delete all sessions owned by this client").clicked() && let Some(worker)=&self.worker {worker.stop.cancel();self.stopping=true;}
                    if self.connected { crate::allowance_ui::allowance_badge(ui, self.account_usage.as_ref()); }
                });
            });
        });
        if let Some(error) = self.error.clone() {
            egui::Panel::top("error_banner")
                .frame(panel_frame())
                .show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(mono(error, 12.0, RED));
                        if icon(ui, "×", "Dismiss error").clicked() {
                            self.error = None;
                        }
                    });
                });
        }
        if self.show_sidebar {
            self.sidebar(ui);
        }
        self.conversation(ui);
    }
}
