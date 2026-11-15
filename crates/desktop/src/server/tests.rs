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
    save_workspace_root(&path, "/data/scratch/first root").unwrap();
    save_workspace_root(&path, "C:\\Users\\Test\\codex-lan-threads").unwrap();
    assert_eq!(
        load_workspace_root(&path).unwrap().as_deref(),
        Some("C:\\Users\\Test\\codex-lan-threads")
    );
    let settings: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(settings.as_object().unwrap().len(), 1);
    assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
    fs::write(&path, b"{invalid settings").unwrap();
    assert_eq!(
        load_workspace_root(&path).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    fs::remove_dir_all(directory).unwrap();
}
