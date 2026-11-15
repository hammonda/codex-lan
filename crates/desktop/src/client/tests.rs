use super::transcript::collapsed_preview;
use super::*;

#[test]
fn json_bodies_follow_streaming_updates_without_changing_copy_text() {
    let mut session = Session::new("thread".into(), "Test".into(), "/tmp/test".into());
    session.agent_text("answer".into(), "{\"value\":".into(), false);
    session.messages[0].prepare_json();
    assert_eq!(session.messages[0].json, Some(None));
    session.agent_text("answer".into(), "1}".into(), false);
    session.messages[0].prepare_json();
    assert_eq!(session.messages[0].json, Some(Some(json!({"value":1}))));
    // A same-length completion can replace content and must invalidate it.
    session.agent_text("answer".into(), "{\"value\":2}".into(), true);
    session.messages[0].prepare_json();
    assert_eq!(session.messages[0].json, Some(Some(json!({"value":2}))));
    assert_eq!(
        session.messages[0].copy_text(MessagePresentation::Full),
        "{\"value\":2}"
    );

    session.native_event(
        "item/commandExecution/outputDelta".into(),
        json!({"itemId":"cmd","delta":"[1"}),
    );
    session.messages[1].prepare_json();
    assert_eq!(session.messages[1].json, Some(None));
    session.native_event(
        "item/completed".into(),
        json!({"item":{"id":"cmd","type":"commandExecution","aggregatedOutput":"[1,2]"}}),
    );
    session.messages[1].prepare_json();
    assert_eq!(session.messages[1].json, Some(Some(json!([1, 2]))));

    let source = "```json\n{\"nested\":[true,null]}\n```";
    let mut fenced = Message::text("AGENT", source);
    fenced.prepare_json();
    assert_eq!(fenced.json, Some(Some(json!({"nested":[true,null]}))));
    assert_eq!(fenced.copy_text(MessagePresentation::Full), source);
    for source in [
        "Normal text",
        "{unfinished",
        "[x] completed task",
        "See this: {\"a\":1}",
    ] {
        let mut message = Message::text("AGENT", source);
        message.prepare_json();
        assert_eq!(message.json, Some(None));
        assert_eq!(message.text, source);
    }
}

#[test]
fn output_levels_reveal_retained_streams_and_copy_only_selected_detail() {
    let mut session = Session::new("thread".into(), "Test".into(), "/tmp/test".into());
    session
        .messages
        .push(Message::text("USER", "Please inspect the files"));
    session.agent_text("commentary".into(), "I am checking the files.".into(), true);
    session.native_event(
        "turn/plan/updated".into(),
        json!({"turnId":"turn","plan":[{"step":"Check files","status":"inProgress"}]}),
    );
    session.native_event(
        "item/reasoning/summaryTextDelta".into(),
        json!({"itemId":"thought","delta":"Emitted reasoning summary"}),
    );
    session.native_event("item/started".into(), json!({"item":{"id":"command","type":"commandExecution","command":"ls","status":"inProgress"}}));
    session.native_event(
        "item/commandExecution/outputDelta".into(),
        json!({"itemId":"command","delta":"FULL_TOOL_OUTPUT\n"}),
    );
    session.native_event(
        "turn/diff/updated".into(),
        json!({"turnId":"turn","diff":"FULL_DIFF_BODY"}),
    );
    session.native_event("custom/progress".into(), json!({"detail":"RAW_EVENT_BODY"}));
    session.agent_text("answer".into(), "The files are ready.".into(), true);
    session
        .messages
        .push(Message::text("ERROR", "Rejected submission"));
    let retained = (session.messages.len(), session.native.len());

    assert_eq!(session.output_level, OutputLevel::Replies);
    let replies = session.visible_output();
    for text in [
        "Please inspect",
        "I am checking",
        "The files are ready",
        "Rejected submission",
    ] {
        assert!(replies.contains(text));
    }
    for text in [
        "Check files",
        "reasoning",
        "FULL_TOOL",
        "FULL_DIFF",
        "RAW_EVENT",
    ] {
        assert!(!replies.contains(text));
    }
    session.set_output_level(OutputLevel::Tools);
    session
        .messages
        .iter_mut()
        .find(|message| message.role == "TOOL")
        .unwrap()
        .expanded = true;
    let tools = session.visible_output();
    for text in ["FULL_TOOL_OUTPUT", "FULL_DIFF_BODY", "RAW_EVENT_BODY"] {
        assert!(tools.contains(text));
    }

    session.set_output_level(OutputLevel::Progress);
    let progress = session.visible_output();
    for text in [
        "Check files",
        "Emitted reasoning summary",
        "ls  inProgress",
        "changes",
        "Rejected submission",
    ] {
        assert!(progress.contains(text));
    }
    for text in ["FULL_TOOL_OUTPUT", "FULL_DIFF_BODY", "RAW_EVENT_BODY"] {
        assert!(!progress.contains(text));
    }
    session
        .messages
        .iter_mut()
        .find(|message| message.role == "TOOL")
        .unwrap()
        .progress_expanded = true;
    assert!(session.visible_output().contains("FULL_TOOL_OUTPUT"));
    session.set_output_level(OutputLevel::Replies);
    assert_eq!(session.visible_output(), replies);
    session.set_output_level(OutputLevel::Everything);
    let everything: Vec<Value> = session
        .visible_output()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(everything.len(), session.native.len() + 2);
    assert!(
        everything
            .iter()
            .any(|record| record["role"] == "ERROR" && record["text"] == "Rejected submission")
    );
    assert!(
        everything
            .iter()
            .any(|record| record["params"]["delta"] == "FULL_TOOL_OUTPUT\n")
    );
    assert_eq!((session.messages.len(), session.native.len()), retained);
}

#[test]
fn item_inspection_matches_only_its_item_or_turn_and_level_changes_close_raw_panels() {
    let mut session = Session::new("thread".into(), "Test".into(), "/tmp/test".into());
    session.native_event(
        "item/started".into(),
        json!({"item":{"id":"tool-a","type":"commandExecution","command":"ls"}}),
    );
    session.native_event(
        "item/commandExecution/outputDelta".into(),
        json!({"itemId":"tool-a","delta":"a"}),
    );
    session.native_event(
        "item/commandExecution/outputDelta".into(),
        json!({"itemId":"tool-b","delta":"b"}),
    );
    session.native_event(
        "item/completed".into(),
        json!({"item":{"id":"answer","type":"agentMessage","text":"Done"}}),
    );
    session.agent_text("answer".into(), "Done".into(), true);
    session.native_event(
        "turn/plan/updated".into(),
        json!({"turnId":"turn-a","plan":[]}),
    );
    session.native_event(
        "turn/plan/updated".into(),
        json!({"turnId":"turn-b","plan":[]}),
    );
    session.native_event(
        "turn/diff/updated".into(),
        json!({"turnId":"turn-a","diff":"diff"}),
    );
    session.native_event("custom/event".into(), json!({"same":"payload"}));
    session.native_event("custom/event".into(), json!({"same":"payload"}));

    let matching = |message: &Message| {
        let scope = message.inspection.as_ref().unwrap();
        session
            .native
            .iter()
            .enumerate()
            .filter_map(|(index, record)| scope.matches(index, record).then_some(index))
            .collect::<Vec<_>>()
    };
    assert_eq!(matching(&session.messages[0]), vec![0, 1]);
    let agent = session
        .messages
        .iter()
        .find(|message| message.role == "AGENT")
        .unwrap();
    assert_eq!(matching(agent), vec![3]);
    let plan = session
        .messages
        .iter()
        .find(|message| message.role == "PLAN")
        .unwrap();
    assert_eq!(matching(plan), vec![4]);
    let diff = session
        .messages
        .iter()
        .find(|message| message.role == "DIFF")
        .unwrap();
    assert_eq!(matching(diff), vec![6]);
    let generic = session
        .messages
        .iter()
        .find(|message| message.role == "EVENT")
        .unwrap();
    assert_eq!(matching(generic), vec![7]);

    session.set_output_level(OutputLevel::Tools);
    session.messages[0].expanded = true;
    for message in &mut session.messages {
        message.inspecting = true;
    }
    session.set_output_level(OutputLevel::Replies);
    assert!(session.messages.iter().all(|message| !message.inspecting));
    assert!(
        session.messages[0].expanded,
        "Tool expansion should survive filtering"
    );
    assert_eq!(session.native.len(), 9);
}

#[test]
fn output_level_changes_do_not_send_commands_or_hide_questions() {
    let mut app = ClientApp::new(false);
    let (tx, mut rx) = mpsc::unbounded_channel();
    app.commands = Some(tx);
    app.handle(ServerEvent::SessionStarted {
        session_id: "thread".into(),
        name: "Test".into(),
        workspace: "/tmp/test".into(),
        model: None,
        effort: None,
    });
    app.handle(ServerEvent::Request {
        session_id: "thread".into(),
        request_id: "question".into(),
        method: "item/tool/requestUserInput".into(),
        params: json!({"questions":[]}),
    });
    for level in OutputLevel::ALL {
        app.sessions[0].set_output_level(level);
        assert_eq!(app.sessions[0].requests.len(), 1);
    }
    assert!(matches!(
        rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
}

#[test]
fn collapsed_output_preview_borrows_a_bounded_unicode_prefix() {
    let text = "界".repeat(1024 * 1024);
    let (preview, more) = collapsed_preview(&text);
    assert!(more);
    assert_eq!(preview.chars().count(), 1800);
    assert_eq!(preview.as_ptr(), text.as_ptr());
    assert_eq!(text.len(), 3 * 1024 * 1024);
    assert_eq!(
        collapsed_preview("a\nb\nc\nd\ne\nf\ng"),
        ("a\nb\nc\nd\ne\nf", true)
    );
    assert_eq!(
        collapsed_preview("a\nb\nc\nd\ne\nf\n"),
        ("a\nb\nc\nd\ne\nf", false)
    );
    assert_eq!(collapsed_preview("small output"), ("small output", false));
}

#[test]
fn completed_agent_item_reconciles_the_streamed_text() {
    let mut session = Session::new("thread".into(), "Test".into(), "/tmp/test".into());
    session.agent_text("item".into(), "Hello ".into(), false);
    session.agent_text("item".into(), "world".into(), false);
    session.agent_text("item".into(), "Hello world!".into(), true);
    assert_eq!(session.messages.len(), 1);
    assert_eq!(session.messages[0].text, "Hello world!");
}

#[test]
fn native_output_is_reconciled_without_losing_streamed_bytes() {
    let mut session = Session::new("thread".into(), "Test".into(), "/tmp/test".into());
    session.native_event("item/started".into(), json!({"item":{"id":"command","type":"commandExecution","command":"ls","status":"inProgress"}}));
    session.native_event(
        "item/commandExecution/outputDelta".into(),
        json!({"itemId":"command","delta":"a\nb\nc\n"}),
    );
    session.native_event("item/completed".into(), json!({"item":{"id":"command","type":"commandExecution","command":"ls","status":"completed","aggregatedOutput":"b\nc\n","exitCode":0}}));
    assert_eq!(session.messages.len(), 1);
    assert_eq!(session.messages[0].text, "a\nb\nc\n");
    assert_eq!(session.native.len(), 3);
    session.native_event(
        "item/completed".into(),
        json!({"item":{"id":"user","type":"userMessage","content":[{"type":"text","text":"ls"}]}}),
    );
    assert_eq!(session.messages.len(), 1);
    assert_eq!(session.native.len(), 4);
}

#[test]
fn rejected_steering_preserves_active_turn_and_restores_prompt() {
    let mut app = ClientApp::new(false);
    app.handle(ServerEvent::SessionStarted {
        session_id: "thread".into(),
        name: "Test".into(),
        workspace: "/tmp/test".into(),
        model: None,
        effort: None,
    });
    app.handle(ServerEvent::TurnStarted {
        session_id: "thread".into(),
    });
    app.sessions[0].pending_prompts = 2;
    app.handle(ServerEvent::PromptRejected {
        session_id: "thread".into(),
        text: "Focus on tests".into(),
        message: "Rejected".into(),
    });
    assert!(app.sessions[0].active);
    assert_eq!(app.sessions[0].pending_prompts, 1);
    assert_eq!(app.sessions[0].draft, "Focus on tests");
    app.handle(ServerEvent::Error {
        session_id: Some("thread".into()),
        message: "Invalid reply".into(),
    });
    assert!(app.sessions[0].active);
    app.handle(ServerEvent::TurnFinished {
        session_id: "thread".into(),
        status: "completed".into(),
    });
    app.handle(ServerEvent::PromptAccepted {
        session_id: "thread".into(),
        steered: true,
    });
    assert!(
        !app.sessions[0].active,
        "late acknowledgement must not revive a completed turn"
    );
    assert_eq!(app.sessions[0].pending_prompts, 0);
}

#[test]
fn sidebar_close_targets_one_session_and_waits_for_deletion_acknowledgement() {
    let mut app = ClientApp::new(false);
    let (tx, mut rx) = mpsc::unbounded_channel();
    app.commands = Some(tx);
    app.connected = true;
    app.sessions = vec![
        Session::new("selected".into(), "Selected".into(), "/tmp/selected".into()),
        Session::new("other".into(), "Other".into(), "/tmp/other".into()),
    ];
    app.selected = Some("selected".into());
    app.sessions[1].active = true;

    app.close_session("other");
    app.close_session("other");
    assert!(
        matches!(rx.try_recv(), Ok(ClientCommand::CloseSession { session_id }) if session_id == "other")
    );
    assert!(matches!(
        rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    assert_eq!(app.selected.as_deref(), Some("selected"));
    assert_eq!(
        app.sessions.len(),
        2,
        "Keep the row until deletion succeeds"
    );
    assert!(!app.sessions[0].closing);
    assert!(app.sessions[1].closing);
    assert!(app.sessions[1].active);

    app.handle(ServerEvent::Error {
        session_id: Some("other".into()),
        message: "Deletion failed; try again".into(),
    });
    assert!(!app.sessions[1].closing);
    assert!(app.sessions[1].active);
    assert_eq!(app.sessions[1].messages.last().unwrap().role, "ERROR");
    app.close_session("other");
    assert!(
        matches!(rx.try_recv(), Ok(ClientCommand::CloseSession { session_id }) if session_id == "other")
    );
    app.handle(ServerEvent::SessionClosed {
        session_id: "other".into(),
    });
    assert_eq!(app.sessions.len(), 1);
    assert_eq!(app.selected.as_deref(), Some("selected"));

    app.close_session("selected");
    assert!(
        matches!(rx.try_recv(), Ok(ClientCommand::CloseSession { session_id }) if session_id == "selected")
    );
    assert_eq!(app.selected.as_deref(), Some("selected"));
    app.handle(ServerEvent::SessionClosed {
        session_id: "selected".into(),
    });
    assert!(app.sessions.is_empty());
    assert!(app.selected.is_none());
}

#[test]
fn sidebar_close_rejects_unavailable_actions_without_stranding_session() {
    let mut app = ClientApp::new(false);
    app.connected = true;
    app.sessions.push(Session::new(
        "thread".into(),
        "Test".into(),
        "/tmp/test".into(),
    ));
    app.close_session("thread");
    assert!(!app.sessions[0].closing);
    assert_eq!(app.error.as_deref(), Some("Connection is unavailable."));

    let (tx, rx) = mpsc::unbounded_channel();
    app.commands = Some(tx);
    drop(rx);
    app.close_session("thread");
    assert!(
        !app.sessions[0].closing,
        "A failed send must leave close available for retry"
    );

    let (tx, mut rx) = mpsc::unbounded_channel();
    app.commands = Some(tx);
    app.close_session("missing");
    app.demo = true;
    app.close_session("thread");
    app.demo = false;
    app.stopping = true;
    app.close_session("thread");
    app.stopping = false;
    app.connected = false;
    app.close_session("thread");
    assert!(matches!(
        rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    assert!(!app.sessions[0].closing);

    app.connected = true;
    app.close_session("thread");
    assert!(
        matches!(rx.try_recv(), Ok(ClientCommand::CloseSession { session_id }) if session_id == "thread")
    );
    assert!(app.sessions[0].closing);
}
