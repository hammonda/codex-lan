use super::*;

fn dashboard_text(app: &mut ServerApp) -> Vec<String> {
    fn collect(shape: &egui::Shape, text: &mut Vec<String>) {
        match shape {
            egui::Shape::Text(shape) => text.push(shape.galley.text().to_owned()),
            egui::Shape::Vec(shapes) => {
                for shape in shapes {
                    collect(shape, text);
                }
            }
            _ => {}
        }
    }
    let ctx = egui::Context::default();
    theme(&ctx);
    let mut text = Vec::new();
    for _ in 0..2 {
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(860.0, 620.0),
                )),
                ..Default::default()
            },
            |ui| app.dashboard(ui),
        );
        text.clear();
        for shape in &output.shapes {
            collect(&shape.shape, &mut text);
        }
        output.textures_delta.clear();
    }
    text
}

fn click_dashboard_label(app: &mut ServerApp, label: &str) {
    let ctx = egui::Context::default();
    theme(&ctx);
    ctx.enable_accesskit();
    let frame = |app: &mut ServerApp, events| {
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(860.0, 620.0),
                )),
                events,
                ..Default::default()
            },
            |ui| app.dashboard(ui),
        );
        output.textures_delta.clear();
        output
    };
    let _ = frame(app, vec![]);
    let output = frame(app, vec![]);
    let bounds = output
        .platform_output
        .accesskit_update
        .unwrap()
        .nodes
        .iter()
        .find_map(|(_, node)| {
            (node.label() == Some(label))
                .then(|| node.bounds())
                .flatten()
        })
        .expect("Session name must expose a selectable target");
    let pos = egui::pos2(
        ((bounds.x0 + bounds.x1) / 2.0) as f32,
        ((bounds.y0 + bounds.y1) / 2.0) as f32,
    );
    for pressed in [true, false] {
        let _ = frame(
            app,
            vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
    }
}

#[test]
fn live_sessions_identify_clients_and_replace_usage_without_log_spam() {
    let mut app = ServerApp::new(false);
    for (id, hostname, peer) in [
        ("one", Some("LAPTOP".into()), "192.168.1.10:40001"),
        ("two", None, "192.168.1.11:40002"),
    ] {
        app.handle(MonitorEvent::ClientConnected {
            id: id.into(),
            hostname,
            peer: peer.into(),
        });
        app.handle(MonitorEvent::SessionOpened {
            client_id: id.into(),
            session_id: id.into(),
            name: "Session 01".into(),
            workspace: format!("/tmp/{id}"),
            model: Some("fixture".into()),
            effort: Some("medium".into()),
        });
    }
    assert!(
        app.logs
            .iter()
            .any(|log| log.text
                == "Opened: Session 01 · LAPTOP (192.168.1.10:40001) · fixture medium")
    );
    assert!(
        app.logs
            .iter()
            .any(|log| log.text == "Opened: Session 01 · 192.168.1.11:40002 · fixture medium")
    );
    let log_count = app.logs.len();
    let update = |id: &str, input, output| MonitorEvent::SessionUsage {
        session_id: id.into(),
        params: serde_json::json!({"tokenUsage":{"total":{"inputTokens":input,"outputTokens":output}}}),
    };
    app.handle(update("one", 15, 6));
    assert!(
        dashboard_text(&mut app)
            .iter()
            .any(|text| text == "in 15 · out 6 · API —")
    );
    app.handle(update("two", 500, 80));
    app.handle(update("one", 30, 12));
    app.handle(update("one", 30, 12));
    app.handle(MonitorEvent::SessionUsage {
        session_id: "one".into(),
        params: serde_json::json!({"tokenUsage":{"last":{"inputTokens":999}}}),
    });
    let text = dashboard_text(&mut app);
    assert!(text.iter().any(|text| text == "in 30 · out 12 · API —"));
    assert!(text.iter().any(|text| text == "in 500 · out 80 · API —"));
    assert!(!text.iter().any(|text| text == "in 15 · out 6 · API —"));
    assert_eq!(app.logs.len(), log_count, "Live totals update in place");
    app.handle(MonitorEvent::ClientDisconnected { id: "one".into() });
    assert_eq!(
        app.sessions["one"].client_label,
        "LAPTOP (192.168.1.10:40001)"
    );
    app.handle(MonitorEvent::SessionClosed {
        session_id: "one".into(),
    });
    assert!(
        app.logs
            .last()
            .unwrap()
            .text
            .contains("LAPTOP (192.168.1.10:40001) · in 30 · out 12")
    );
    app.handle(update("one", 100, 100));
    assert_eq!(
        app.sessions.len(),
        1,
        "Late events cannot recreate a closed session"
    );
    assert!(app.sessions.contains_key("two"));
}

#[test]
fn workspace_preference_survives_replacement_and_rejects_corruption() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "codex-lan-preferences-test-{}-{nonce}",
        std::process::id()
    ));
    let path = directory.join("server-preferences.json");
    assert_eq!(load_workspace_root(&path).unwrap(), None);
    save_workspace_root(&path, "/data/scratch/first root", None).unwrap();
    save_workspace_root(&path, "C:\\Users\\Test\\codex-lan-threads", None).unwrap();
    assert_eq!(
        load_workspace_root(&path).unwrap().as_deref(),
        Some("C:\\Users\\Test\\codex-lan-threads")
    );
    let settings: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(settings.as_object().unwrap().len(), 1);
    assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
    let connection = directory.join("local-connection.json");
    save_workspace_root(&path, "/data/scratch", Some(&connection)).unwrap();
    assert_eq!(load_connection_file(&path), Some(connection));
    fs::write(&path, b"{invalid settings").unwrap();
    assert_eq!(
        load_workspace_root(&path).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn inspector_shows_both_directions_of_only_the_selected_session_and_keeps_closed_output() {
    use codex_lan_core::protocol::{ClientCommand, ServerEvent};
    let mut app = ServerApp::new(false);
    for id in ["one", "two"] {
        app.handle(MonitorEvent::SessionOpened {
            client_id: id.into(),
            session_id: id.into(),
            name: format!("Claude / {id}"),
            workspace: format!("/tmp/{id}"),
            model: None,
            effort: None,
        });
        app.handle(MonitorEvent::SessionCommand {
            session_id: id.into(),
            command: ClientCommand::Prompt {
                session_id: id.into(),
                text: format!("Prompt {id}"),
            },
        });
        app.handle(MonitorEvent::SessionEvent(ServerEvent::Delta {
            session_id: id.into(),
            item_id: "answer".into(),
            text: "partial".into(),
        }));
        app.handle(MonitorEvent::SessionEvent(ServerEvent::ItemCompleted {
            session_id: id.into(),
            item_id: "answer".into(),
            text: format!("Answer {id}"),
        }));
    }
    click_dashboard_label(&mut app, "Claude / one");
    assert_eq!(app.inspected.as_deref(), Some("one"));
    let text = dashboard_text(&mut app).join("\n");
    assert!(text.contains("Prompt one") && text.contains("Answer one"));
    assert!(
        !text.contains("Prompt two") && !text.contains("Answer two") && !text.contains("partial")
    );
    app.handle(MonitorEvent::SessionClosed {
        session_id: "one".into(),
    });
    assert!(!app.sessions.contains_key("one"));
    assert_eq!(app.recent.len(), 1);
    assert!(dashboard_text(&mut app).join("\n").contains("Answer one"));
    app.inspected = Some("two".into());
    let text = dashboard_text(&mut app).join("\n");
    assert!(text.contains("Answer two") && !text.contains("Answer one"));
}

#[test]
fn local_connection_rotates_privately_and_maps_wildcard_listeners_to_loopback() {
    let directory = std::env::temp_dir().join(format!(
        "codex-lan-connection-test-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let path = directory.join("connection.json");
    for (listener, expected, pairing) in [
        ("0.0.0.0:7331", "127.0.0.1:7331", "first-private-code"),
        ("[::]:7331", "[::1]:7331", "second-private-code"),
        (
            "192.168.1.12:7340",
            "192.168.1.12:7340",
            "third-private-code",
        ),
    ] {
        save_local_connection(&path, listener, pairing).unwrap();
        let contents: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            contents,
            serde_json::json!({"address":expected,"pairing_code":pairing})
        );
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
    assert!(save_local_connection(&path, "invalid-address", "unused").is_err());
    assert!(
        fs::read_to_string(&path)
            .unwrap()
            .contains("third-private-code")
    );
    fs::remove_dir_all(directory).unwrap();
}
