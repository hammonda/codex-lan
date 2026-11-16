use super::*;

fn observer() -> SessionObserver {
    SessionObserver::new("one".into(), "Claude / Task".into(), "/tmp/context".into())
}

fn prompt(observer: &mut SessionObserver, text: &str) {
    observer.command(ClientCommand::Prompt {
        session_id: "one".into(),
        text: text.into(),
    });
}

fn delta(observer: &mut SessionObserver, text: &str) {
    observer.event(ServerEvent::Delta {
        session_id: "one".into(),
        item_id: "reply".into(),
        text: text.into(),
    });
}

fn native(observer: &mut SessionObserver, method: &str, params: Value) {
    observer.event(ServerEvent::NativeEvent {
        session_id: "one".into(),
        method: method.into(),
        params,
    });
}

#[test]
fn steering_splits_streams_in_order_and_completion_does_not_duplicate_the_reply() {
    let mut observer = observer();
    prompt(&mut observer, "original task");
    delta(&mut observer, "before steering ");
    prompt(&mut observer, "changed scope");
    delta(&mut observer, "after steering");
    observer.event(ServerEvent::ItemCompleted {
        session_id: "one".into(),
        item_id: "reply".into(),
        text: "before steering after steering — done".into(),
    });
    assert_eq!(
        observer
            .rows
            .iter()
            .map(|row| row.message.text.as_str())
            .collect::<Vec<_>>(),
        [
            "original task",
            "before steering ",
            "changed scope",
            "after steering — done"
        ]
    );
    assert_eq!(
        observer
            .rows
            .iter()
            .map(|row| row.stamp.incoming)
            .collect::<Vec<_>>(),
        [true, false, true, false]
    );
    assert!(
        observer
            .rows
            .windows(2)
            .all(|pair| pair[0].stamp.sequence < pair[1].stamp.sequence)
    );
    let text = observer.visible_output();
    assert!(text.find("before steering").unwrap() < text.find("changed scope").unwrap());
    assert!(text.find("changed scope").unwrap() < text.find("after steering").unwrap());
    assert!(text.contains("Claude → Codex") && text.contains("Codex → Claude"));
    observer.session.output_level = OutputLevel::Everything;
    let raw: Vec<Value> = observer
        .visible_output()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(raw.len(), 5);
    assert_eq!(raw[2]["method"], "client/prompt");
    assert_eq!(raw[4]["method"], "bridge/item_completed");
    assert!(
        raw.windows(2)
            .all(|pair| pair[0]["sequence"].as_u64() < pair[1]["sequence"].as_u64())
    );
}

#[test]
fn tools_questions_and_revised_snapshots_keep_their_arrival_positions() {
    let mut observer = observer();
    prompt(&mut observer, "task");
    delta(&mut observer, "initial answer");
    native(
        &mut observer,
        "item/started",
        json!({"item":{"id":"tool","type":"commandExecution","command":"python - <<'PY'\nprint('hello')\nPY"}}),
    );
    native(
        &mut observer,
        "item/commandExecution/outputDelta",
        json!({"itemId":"tool","delta":"first\n"}),
    );
    observer.event(ServerEvent::Request {
        session_id: "one".into(),
        request_id: "q".into(),
        method: "question".into(),
        params: json!({"question":"Continue?"}),
    });
    observer.command(ClientCommand::Reply {
        session_id: "one".into(),
        request_id: "q".into(),
        result: json!({"answer":"yes"}),
    });
    native(
        &mut observer,
        "item/commandExecution/outputDelta",
        json!({"itemId":"tool","delta":"second\n"}),
    );
    native(
        &mut observer,
        "item/completed",
        json!({"item":{"id":"tool","type":"commandExecution","exitCode":0,"aggregatedOutput":"first\nsecond\n"}}),
    );
    observer.event(ServerEvent::ItemCompleted {
        session_id: "one".into(),
        item_id: "reply".into(),
        text: "corrected answer".into(),
    });
    assert_eq!(observer.rows[1].message.text, "initial answer");
    assert_eq!(
        observer.rows.last().unwrap().message.text,
        "corrected answer"
    );
    assert!(
        observer
            .rows
            .last()
            .unwrap()
            .message
            .title
            .contains("revised snapshot")
    );
    assert_eq!(
        observer
            .rows
            .iter()
            .filter(|row| row.message.text == "first\n")
            .count(),
        1
    );
    assert_eq!(
        observer
            .rows
            .iter()
            .filter(|row| row.message.text == "second\n")
            .count(),
        1
    );
    assert!(
        observer
            .rows
            .iter()
            .find(|row| row.message.title.contains("exit 0"))
            .unwrap()
            .message
            .text
            .is_empty()
    );
    assert!(
        observer
            .rows
            .iter()
            .all(|row| !row.message.title.contains('\n'))
    );
    assert!(observer.session.messages.is_empty());
}

#[test]
fn retained_history_resets_all_indexes_and_keeps_sequence_numbers_monotonic() {
    let mut observer = observer();
    prompt(&mut observer, "old");
    delta(&mut observer, "old reply");
    let previous = observer.sequence;
    observer.bytes = MAX_BYTES;
    native(
        &mut observer,
        "fixture/detail",
        json!({"keep":"new detail"}),
    );
    assert!(observer.discarded);
    assert_eq!(observer.session.native.len(), 1);
    assert_eq!(observer.stamps.len(), 1);
    assert!(observer.rows.iter().all(|row| row.records == [0]));
    assert!(observer.rows[0].stamp.sequence > previous);
    assert!(observer.streams.is_empty());
    assert!(!observer.retain(MAX_BYTES + 1));
    assert!(observer.bytes <= MAX_BYTES);
}

fn frame(
    observer: &mut SessionObserver,
    ctx: &egui::Context,
    time: f64,
    events: Vec<egui::Event>,
) -> (egui::FullOutput, egui::scroll_area::ScrollAreaOutput<()>) {
    let mut scroll = None;
    let mut output = ctx.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(500.0, 360.0),
            )),
            time: Some(time),
            events,
            ..Default::default()
        },
        |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                scroll = Some(observer.ui(ui, false));
            });
        },
    );
    output.textures_delta.clear();
    (output, scroll.unwrap())
}

#[test]
fn long_activity_cannot_displace_controls_and_scroll_pauses_until_latest_is_clicked() {
    let mut observer = observer();
    observer.session.activity = format!(
        "Finished commandExecution python {}",
        "long command\n".repeat(400)
    );
    for index in 0..30 {
        prompt(
            &mut observer,
            &format!("Prompt {index}: {}", "longword".repeat(100)),
        );
        observer.event(ServerEvent::ItemCompleted {
            session_id: "one".into(),
            item_id: format!("reply-{index}"),
            text: format!("Reply {index}"),
        });
    }
    let ctx = egui::Context::default();
    theme(&ctx);
    ctx.enable_accesskit();
    let mut time = 0.0;
    let mut next = |observer: &mut SessionObserver, events| {
        time += 0.1;
        frame(observer, &ctx, time, events)
    };
    let _ = next(&mut observer, vec![]);
    let (_, scroll) = next(&mut observer, vec![]);
    assert!(
        scroll.inner_rect.top() < 65.0,
        "Status must stay on one line"
    );
    assert!(scroll.inner_rect.height() > 290.0);
    assert!(
        scroll.content_size.x <= scroll.inner_rect.width() + 1.0,
        "Transcript must wrap, not widen its viewport"
    );
    let bottom = scroll.state.clamped_offset().y;
    assert!(bottom > 100.0);
    let (_, _) = next(
        &mut observer,
        vec![
            egui::Event::PointerMoved(scroll.inner_rect.center()),
            egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                phase: egui::TouchPhase::Move,
                source: egui::MouseWheelSource::Unknown,
                delta: egui::vec2(0.0, 500.0),
                modifiers: egui::Modifiers::NONE,
            },
        ],
    );
    for _ in 0..8 {
        let _ = next(&mut observer, vec![]);
    }
    let (_, scroll) = next(&mut observer, vec![]);
    let paused = scroll.state.clamped_offset().y;
    assert!(paused < bottom - 100.0 && !observer.at_bottom);
    prompt(&mut observer, "new message while reading history");
    let (output, scroll) = next(&mut observer, vec![]);
    assert!(
        (scroll.state.clamped_offset().y - paused).abs() < 1.0,
        "New output must not pull the reader down"
    );
    let bounds = output
        .platform_output
        .accesskit_update
        .unwrap()
        .nodes
        .iter()
        .find_map(|(_, node)| {
            (node.label() == Some("↓ latest"))
                .then(|| node.bounds())
                .flatten()
        })
        .unwrap();
    let position = egui::pos2(
        ((bounds.x0 + bounds.x1) / 2.0) as f32,
        ((bounds.y0 + bounds.y1) / 2.0) as f32,
    );
    for pressed in [true, false] {
        let _ = next(
            &mut observer,
            vec![
                egui::Event::PointerMoved(position),
                egui::Event::PointerButton {
                    pos: position,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
    }
    for _ in 0..10 {
        let _ = next(&mut observer, vec![]);
    }
    assert!(
        observer.at_bottom,
        "Latest must restore following at the bottom"
    );
}
