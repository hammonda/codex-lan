use super::transcript::message_ui;
use super::*;

fn sidebar_frame(
    ctx: &egui::Context,
    app: &mut ClientApp,
    events: Vec<egui::Event>,
) -> Vec<(String, egui::Rect)> {
    let input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(860.0, 620.0),
        )),
        events,
        ..Default::default()
    };
    let mut output = ctx.run_ui(input, |ui| app.sidebar(ui));
    let widgets = output
        .platform_output
        .accesskit_update
        .take()
        .unwrap()
        .nodes
        .into_iter()
        .filter_map(|(_, node)| {
            let label = node.label()?.to_owned();
            let rect = node.bounds()?;
            Some((
                label,
                egui::Rect::from_min_max(
                    egui::pos2(rect.x0 as f32, rect.y0 as f32),
                    egui::pos2(rect.x1 as f32, rect.y1 as f32),
                ),
            ))
        })
        .collect();
    output.textures_delta.clear();
    widgets
}

fn click_sidebar(ctx: &egui::Context, app: &mut ClientApp, pos: egui::Pos2) {
    for pressed in [true, false] {
        sidebar_frame(
            ctx,
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
fn embedded_close_hit_target_does_not_activate_the_session_switcher() {
    let ctx = egui::Context::default();
    theme(&ctx);
    ctx.enable_accesskit();
    let mut app = ClientApp::new(false);
    let (tx, mut rx) = mpsc::unbounded_channel();
    app.commands = Some(tx);
    app.connected = true;
    app.sessions = vec![
        Session::new("selected".into(), "Selected".into(), "/tmp/selected".into()),
        Session::new("other".into(), "Other".into(), "/tmp/other".into()),
    ];
    app.selected = Some("selected".into());
    sidebar_frame(&ctx, &mut app, vec![]);
    let widgets = sidebar_frame(&ctx, &mut app, vec![]);
    let close = widgets
        .iter()
        .find(|(label, _)| label.starts_with("Close Other and delete"))
        .expect("The close icon must expose its own accessible hit target")
        .1;
    let switcher = widgets
        .iter()
        .find(|(label, _)| label == "· Other")
        .expect("The session switcher must remain a separate button")
        .1;
    assert!(!close.intersects(switcher));
    assert!(close.center().x > switcher.right());
    assert!((close.center().y - switcher.center().y).abs() < 2.0);

    click_sidebar(&ctx, &mut app, close.center());
    assert_eq!(app.selected.as_deref(), Some("selected"));
    assert!(app.sessions[1].closing);
    assert!(
        matches!(rx.try_recv(), Ok(ClientCommand::CloseSession { session_id }) if session_id == "other")
    );
    assert!(matches!(
        rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));

    app.handle(ServerEvent::Error {
        session_id: Some("other".into()),
        message: "Test retry".into(),
    });
    click_sidebar(&ctx, &mut app, switcher.center());
    assert_eq!(app.selected.as_deref(), Some("other"));
    assert!(!app.sessions[1].closing);
    assert!(matches!(
        rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
}

fn composer_frame(
    ctx: &egui::Context,
    app: &mut ClientApp,
    events: Vec<egui::Event>,
) -> egui::Rect {
    let mut output = ctx.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(860.0, 620.0),
            )),
            events,
            ..Default::default()
        },
        |ui| app.conversation(ui),
    );
    let rect = output
        .platform_output
        .accesskit_update
        .take()
        .unwrap()
        .nodes
        .into_iter()
        .find_map(|(_, node)| {
            (node.role() == egui::accesskit::Role::MultilineTextInput)
                .then(|| node.bounds())
                .flatten()
        })
        .expect("The prompt must be a multiline text input");
    output.textures_delta.clear();
    egui::Rect::from_min_max(
        egui::pos2(rect.x0 as f32, rect.y0 as f32),
        egui::pos2(rect.x1 as f32, rect.y1 as f32),
    )
}

#[test]
fn enter_sends_and_shift_enter_edits_multiline_prompts_for_idle_and_active_sessions() {
    let enter = |modifiers| {
        vec![true, false]
            .into_iter()
            .map(|pressed| egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed,
                repeat: false,
                modifiers,
            })
            .collect()
    };
    for active in [false, true] {
        let ctx = egui::Context::default();
        theme(&ctx);
        ctx.enable_accesskit();
        let mut app = ClientApp::new(false);
        let (tx, mut rx) = mpsc::unbounded_channel();
        app.commands = Some(tx);
        app.connected = true;
        let mut session = Session::new("test".into(), "Test".into(), "/tmp/test".into());
        session.active = active;
        app.selected = Some(session.id.clone());
        app.sessions.push(session);
        composer_frame(&ctx, &mut app, vec![]);
        let input = composer_frame(&ctx, &mut app, vec![]);
        for pressed in [true, false] {
            composer_frame(
                &ctx,
                &mut app,
                vec![
                    egui::Event::PointerMoved(input.center()),
                    egui::Event::PointerButton {
                        pos: input.center(),
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            );
        }
        let focus = ctx.memory(|memory| memory.focused()).unwrap();
        composer_frame(&ctx, &mut app, vec![egui::Event::Text("First line".into())]);
        composer_frame(&ctx, &mut app, enter(egui::Modifiers::SHIFT));
        composer_frame(
            &ctx,
            &mut app,
            vec![egui::Event::Text("Second line".into())],
        );
        assert_eq!(app.sessions[0].draft, "First line\nSecond line");
        assert!(rx.try_recv().is_err(), "Shift+Enter must not send");
        composer_frame(&ctx, &mut app, enter(egui::Modifiers::NONE));
        assert!(
            matches!(rx.try_recv(), Ok(ClientCommand::Prompt { session_id, text })
            if session_id == "test" && text == "First line\nSecond line")
        );
        assert!(app.sessions[0].draft.is_empty());
        assert_eq!(ctx.memory(|memory| memory.focused()), Some(focus));
        composer_frame(&ctx, &mut app, enter(egui::Modifiers::NONE));
        assert!(rx.try_recv().is_err(), "An empty draft must not send");
        assert!(
            app.sessions[0].draft.is_empty(),
            "Enter must not insert a newline"
        );
        composer_frame(
            &ctx,
            &mut app,
            vec![egui::Event::Paste("Keep\npasted lines".into())],
        );
        assert!(
            rx.try_recv().is_err(),
            "Pasting multiline text must not send"
        );
        ctx.memory_mut(|memory| memory.surrender_focus(focus));
        composer_frame(&ctx, &mut app, enter(egui::Modifiers::NONE));
        assert_eq!(app.sessions[0].draft, "Keep\npasted lines");
        assert!(
            rx.try_recv().is_err(),
            "Enter outside the prompt must not send"
        );
        ctx.memory_mut(|memory| memory.request_focus(focus));
        composer_frame(&ctx, &mut app, enter(egui::Modifiers::CTRL));
        assert!(
            matches!(rx.try_recv(), Ok(ClientCommand::Prompt { text, .. })
            if text == "Keep\npasted lines"),
            "The existing Ctrl+Enter shortcut remains usable"
        );
        assert!(
            rx.try_recv().is_err(),
            "One key press must enqueue just one prompt"
        );
    }
}

struct MessageFrame {
    labels: Vec<(String, egui::Rect)>,
    painted: Vec<String>,
    text_rects: Vec<(String, egui::Rect)>,
}

fn message_frame(
    ctx: &egui::Context,
    message: &mut Message,
    presentation: MessagePresentation,
    native: &[NativeRecord],
    events: Vec<egui::Event>,
) -> MessageFrame {
    let mut output = ctx.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(860.0, 620.0),
            )),
            events,
            ..Default::default()
        },
        |ui| {
            egui::CentralPanel::default()
                .show(ui, |ui| message_ui(ui, message, presentation, native));
        },
    );
    let labels = output
        .platform_output
        .accesskit_update
        .take()
        .unwrap()
        .nodes
        .into_iter()
        .filter_map(|(_, node)| {
            let label = node.label()?.to_owned();
            let rect = node.bounds()?;
            Some((
                label,
                egui::Rect::from_min_max(
                    egui::pos2(rect.x0 as f32, rect.y0 as f32),
                    egui::pos2(rect.x1 as f32, rect.y1 as f32),
                ),
            ))
        })
        .collect();
    fn collect_text(
        shape: &egui::Shape,
        result: &mut Vec<String>,
        rects: &mut Vec<(String, egui::Rect)>,
    ) {
        match shape {
            egui::Shape::Text(text) => {
                result.push(text.galley.text().into());
                rects.push((
                    text.galley.text().into(),
                    text.galley.rect.translate(text.pos.to_vec2()),
                ));
            }
            egui::Shape::Vec(shapes) => {
                for shape in shapes {
                    collect_text(shape, result, rects);
                }
            }
            _ => {}
        }
    }
    let mut painted = Vec::new();
    let mut text_rects = Vec::new();
    for shape in &output.shapes {
        collect_text(&shape.shape, &mut painted, &mut text_rects);
    }
    output.textures_delta.clear();
    MessageFrame {
        labels,
        painted,
        text_rects,
    }
}

fn click_message(
    ctx: &egui::Context,
    message: &mut Message,
    presentation: MessagePresentation,
    native: &[NativeRecord],
    label: &str,
) -> MessageFrame {
    let frame = message_frame(ctx, message, presentation, native, vec![]);
    let pos = frame
        .labels
        .iter()
        .find(|(text, _)| text == label)
        .unwrap_or_else(|| panic!("Missing control {label}: {:?}", frame.labels))
        .1
        .center();
    for pressed in [true, false] {
        message_frame(
            ctx,
            message,
            presentation,
            native,
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
    message_frame(ctx, message, presentation, native, vec![])
}

#[test]
fn reasoning_disclosure_hides_streamed_text_and_any_open_json_inspector() {
    for level in [OutputLevel::Progress, OutputLevel::Tools] {
        let ctx = egui::Context::default();
        theme(&ctx);
        ctx.enable_accesskit();
        let mut session = Session::new("thread".into(), "Test".into(), "/tmp/test".into());
        session.native_event(
            "item/reasoning/summaryTextDelta".into(),
            json!({"itemId":"thought","delta":"Checking a file."}),
        );
        let message = &mut session.messages[0];
        let presentation = level.presentation(message);
        message_frame(&ctx, message, presentation, &session.native, vec![]);
        assert!(
            message_frame(&ctx, message, presentation, &session.native, vec![])
                .painted
                .iter()
                .any(|text| text == "Checking a file.")
        );
        let closed = click_message(
            &ctx,
            message,
            presentation,
            &session.native,
            "Collapse output",
        );
        assert!(!message.expanded);
        assert!(
            !closed
                .painted
                .iter()
                .any(|text| text.contains("Checking a file."))
        );
        message.append_text(" Still working.");
        assert!(
            !message_frame(&ctx, message, presentation, &session.native, vec![])
                .painted
                .iter()
                .any(|text| text.contains("Still working"))
        );
        let expanded = click_message(
            &ctx,
            message,
            presentation,
            &session.native,
            "Expand output",
        );
        assert!(
            expanded
                .painted
                .iter()
                .any(|text| text == "Checking a file. Still working.")
        );
        let json = click_message(&ctx, message, presentation, &session.native, "JSON");
        assert!(message.inspecting && message.expanded);
        assert!(json.painted.iter().any(|text| text == "raw Codex events"));
        assert!(
            json.painted
                .iter()
                .any(|text| text.contains("item/reasoning/summaryTextDelta"))
        );
        let closed = click_message(
            &ctx,
            message,
            presentation,
            &session.native,
            "Collapse output",
        );
        assert!(!closed.painted.iter().any(|text| text == "raw Codex events"));
        assert!(
            !closed
                .painted
                .iter()
                .any(|text| text.contains("Still working"))
        );
        assert!(
            message
                .copy_text(presentation)
                .contains("Checking a file. Still working.")
        );
        let no_json = message_frame(&ctx, message, presentation, &session.native, vec![]);
        assert!(!message.inspecting);
        assert!(
            !no_json
                .painted
                .iter()
                .any(|text| text == "raw Codex events")
        );
    }
}

#[test]
fn body_collapse_is_distinct_from_long_output_preview_and_json_tree_expansion() {
    let ctx = egui::Context::default();
    theme(&ctx);
    ctx.enable_accesskit();
    let mut message = Message::text("TOOL", "one\ntwo\nthree\nfour\nfive\nsix\nlast line");
    message.title = "command output".into();
    let full = MessagePresentation::Full;
    message_frame(&ctx, &mut message, full, &[], vec![]);
    let preview = message_frame(&ctx, &mut message, full, &[], vec![]);
    assert!(preview.painted.iter().any(|text| text.contains("six")));
    assert!(
        !preview
            .painted
            .iter()
            .any(|text| text.contains("last line"))
    );
    let more = click_message(&ctx, &mut message, full, &[], "…");
    assert!(more.painted.iter().any(|text| text.contains("last line")));
    let closed = click_message(&ctx, &mut message, full, &[], "Collapse output");
    assert!(!closed.painted.iter().any(|text| text.contains("one")));
    assert!(message.full_text);
    let reopened = click_message(&ctx, &mut message, full, &[], "Expand output");
    assert!(
        reopened
            .painted
            .iter()
            .any(|text| text.contains("last line"))
    );

    message.replace_text("{\"nested\":{\"value\":1}}".into());
    message_frame(&ctx, &mut message, full, &[], vec![]);
    let closed = click_message(&ctx, &mut message, full, &[], "Collapse output");
    assert!(!closed.painted.iter().any(|text| text.contains("nested")));
    let reopened = click_message(&ctx, &mut message, full, &[], "Expand output");
    assert!(reopened.painted.iter().any(|text| text.contains("nested")));
}

#[test]
fn reasoning_without_text_discloses_its_available_native_events() {
    for level in [OutputLevel::Progress, OutputLevel::Tools] {
        for item in [
            json!({"id":"thought", "type":"reasoning"}),
            json!({"id":"thought", "type":"reasoning", "summary":[], "content":[]}),
            json!({"id":"thought", "type":"reasoning", "summary":[" \n"], "content":[]}),
        ] {
            let ctx = egui::Context::default();
            theme(&ctx);
            ctx.enable_accesskit();
            let mut session = Session::new("thread".into(), "Test".into(), "/tmp/test".into());
            for method in ["item/started", "item/completed"] {
                session.native_event(method.into(), json!({"item":item.clone()}));
            }
            assert_eq!(session.messages.len(), 1);
            let message = &mut session.messages[0];
            let presentation = level.presentation(message);
            message_frame(&ctx, message, presentation, &session.native, vec![]);
            let initial = message_frame(&ctx, message, presentation, &session.native, vec![]);
            assert!(
                !initial
                    .painted
                    .iter()
                    .any(|text| text == "raw Codex events")
            );
            let opened = click_message(
                &ctx,
                message,
                presentation,
                &session.native,
                "Expand output",
            );
            assert!(opened.painted.iter().any(|text| text == "raw Codex events"));
            assert!(
                opened
                    .painted
                    .iter()
                    .any(|text| text.contains("item/completed"))
            );
            let closed = click_message(
                &ctx,
                message,
                presentation,
                &session.native,
                "Collapse output",
            );
            assert!(!closed.painted.iter().any(|text| text == "raw Codex events"));
            let opened = click_message(&ctx, message, presentation, &session.native, "JSON");
            assert!(opened.painted.iter().any(|text| text == "raw Codex events"));
            let closed = click_message(
                &ctx,
                message,
                presentation,
                &session.native,
                "Collapse output",
            );
            assert!(!closed.painted.iter().any(|text| text == "raw Codex events"));
        }
    }
}

#[test]
fn reasoning_text_arriving_after_metadata_respects_collapse_and_reopens_readably() {
    for snapshot in [false, true] {
        let ctx = egui::Context::default();
        theme(&ctx);
        ctx.enable_accesskit();
        let mut session = Session::new("thread".into(), "Test".into(), "/tmp/test".into());
        session.native_event(
            "item/started".into(),
            json!({"item":{"id":"thought", "type":"reasoning"}}),
        );
        let full = MessagePresentation::Full;
        message_frame(
            &ctx,
            &mut session.messages[0],
            full,
            &session.native,
            vec![],
        );
        click_message(
            &ctx,
            &mut session.messages[0],
            full,
            &session.native,
            "Expand output",
        );
        click_message(
            &ctx,
            &mut session.messages[0],
            full,
            &session.native,
            "Collapse output",
        );
        let text = "Checking the received file.";
        if snapshot {
            session.native_event("item/completed".into(), json!({"item":{"id":"thought", "type":"reasoning", "summary":[text], "content":[]}}));
        } else {
            session.native_event(
                "item/reasoning/summaryTextDelta".into(),
                json!({"itemId":"thought", "summaryIndex":0, "delta":text}),
            );
        }
        assert_eq!(session.messages.len(), 1);
        let message = &mut session.messages[0];
        let closed = message_frame(&ctx, message, full, &session.native, vec![]);
        assert!(
            !closed
                .painted
                .iter()
                .any(|painted| painted == text || painted == "raw Codex events")
        );
        let opened = click_message(&ctx, message, full, &session.native, "Expand output");
        assert!(opened.painted.iter().any(|painted| painted == text));
        assert!(
            !opened
                .painted
                .iter()
                .any(|painted| painted == "raw Codex events")
        );
    }
}

#[test]
fn submitted_prompt_starts_beside_its_marker_and_keeps_copy_and_wrapping() {
    for prompt in [
        "Download the datasheet.".to_owned(),
        "First line\nSecond line\n第三行".to_owned(),
        "A long prompt with words that must wrap. ".repeat(20),
    ] {
        let ctx = egui::Context::default();
        theme(&ctx);
        ctx.enable_accesskit();
        let mut message = Message::text("USER", &prompt);
        message_frame(&ctx, &mut message, MessagePresentation::Full, &[], vec![]);
        let frame = message_frame(&ctx, &mut message, MessagePresentation::Full, &[], vec![]);
        let marker = frame
            .text_rects
            .iter()
            .find(|(text, _)| text == ">" || text == "›")
            .unwrap()
            .1;
        let body = frame
            .text_rects
            .iter()
            .find(|(text, _)| text == &prompt)
            .unwrap()
            .1;
        assert!(
            (marker.top() - body.top()).abs() < 2.0,
            "marker {marker:?}, prompt {body:?}"
        );
        assert!(body.left() > marker.right());
        assert!(
            body.right() <= 826.0,
            "prompt must leave room for copying: {body:?}"
        );
        assert_eq!(message.copy_text(MessagePresentation::Full), prompt);
        if prompt.contains('\n') || prompt.len() > 100 {
            assert!(
                body.height() > 20.0,
                "multiline prompts must stay multiline"
            );
        }
    }
}
