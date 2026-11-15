#![cfg(feature = "test-support")]

mod support;

use codex_lan_core::{
    bridge::{BridgeConfig, CodexBridge},
    protocol::{ClientCommand, ServerEvent, SessionOptions},
    transport::{ClientConfig, MonitorEvent, ServerConfig, run_client, run_server},
};
use std::{path::Path, time::Duration};
use support::calls;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

async fn event(
    rx: &mut mpsc::UnboundedReceiver<ServerEvent>,
    predicate: impl Fn(&ServerEvent) -> bool,
) -> ServerEvent {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let e = rx.recv().await.expect("event stream closed");
            if predicate(&e) {
                return e;
            }
        }
    })
    .await
    .expect("timed out waiting for client event")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn encrypted_lan_sessions_stream_isolate_and_delete() {
    let temp = tempfile::tempdir().unwrap();
    let (monitor_tx, mut monitor_rx) = mpsc::unbounded_channel();
    let mut monitor_events = Vec::new();
    let server_stop = CancellationToken::new();
    let server = tokio::spawn(run_server(
        ServerConfig {
            bind: "127.0.0.1:0".into(),
            workspace: temp.path().into(),
            codex_path: env!("CARGO_BIN_EXE_fake-codex").into(),
            state_dir: temp.path().join("state"),
            model: None,
        },
        monitor_tx,
        server_stop.clone(),
    ));
    let (address, pairing_code) = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let MonitorEvent::Ready {
                address,
                pairing_code,
                ..
            } = monitor_rx.recv().await.unwrap()
            {
                break (address, pairing_code);
            }
        }
    })
    .await
    .unwrap();
    let (cmd_a, rx_a) = mpsc::unbounded_channel();
    let (ev_a, mut events_a) = mpsc::unbounded_channel();
    let stop_a = CancellationToken::new();
    let a = tokio::spawn(run_client(
        ClientConfig {
            address: address.clone(),
            pairing_code: pairing_code.clone(),
        },
        rx_a,
        ev_a,
        stop_a.clone(),
    ));
    event(&mut events_a, |e| matches!(e, ServerEvent::Welcome { .. })).await;
    cmd_a
        .send(ClientCommand::NewSession {
            name: "First".into(),
            options: Default::default(),
        })
        .unwrap();
    let ServerEvent::SessionStarted {
        session_id: first,
        workspace: first_workspace,
        ..
    } = event(&mut events_a, |e| {
        matches!(e, ServerEvent::SessionStarted { .. })
    })
    .await
    else {
        unreachable!()
    };
    cmd_a
        .send(ClientCommand::Prompt {
            session_id: first.clone(),
            text: "hello".into(),
        })
        .unwrap();
    let mut text = String::new();
    loop {
        let e = event(&mut events_a, |_| true).await;
        match e {
            ServerEvent::Delta { text: part, .. } => text.push_str(&part),
            ServerEvent::TurnFinished { status, .. } => {
                assert_eq!(status, "completed");
                break;
            }
            ServerEvent::Error { message, .. } => panic!("unexpected error: {message}"),
            _ => {}
        }
    }
    assert_eq!(text, "Hello from Codex.");
    let (cmd_b, rx_b) = mpsc::unbounded_channel();
    let (ev_b, mut events_b) = mpsc::unbounded_channel();
    let stop_b = CancellationToken::new();
    let b = tokio::spawn(run_client(
        ClientConfig {
            address,
            pairing_code,
        },
        rx_b,
        ev_b,
        stop_b.clone(),
    ));
    event(&mut events_b, |e| matches!(e, ServerEvent::Welcome { .. })).await;
    cmd_b
        .send(ClientCommand::Prompt {
            session_id: first.clone(),
            text: "must be rejected".into(),
        })
        .unwrap();
    event(&mut events_b, |e| matches!(e, ServerEvent::Error { .. })).await;
    cmd_b
        .send(ClientCommand::NewSession {
            name: "Second".into(),
            options: Default::default(),
        })
        .unwrap();
    let ServerEvent::SessionStarted {
        session_id: second,
        workspace: second_workspace,
        ..
    } = event(&mut events_b, |e| {
        matches!(e, ServerEvent::SessionStarted { .. })
    })
    .await
    else {
        unreachable!()
    };
    let root = temp.path().canonicalize().unwrap();
    assert_eq!(Path::new(&first_workspace).parent(), Some(root.as_path()));
    assert_eq!(Path::new(&second_workspace).parent(), Some(root.as_path()));
    assert_ne!(first_workspace, second_workspace);
    cmd_b
        .send(ClientCommand::Prompt {
            session_id: second.clone(),
            text: "wait until interrupted".into(),
        })
        .unwrap();
    event(&mut events_b, |e| {
        matches!(e, ServerEvent::TurnStarted { .. })
    })
    .await;
    // Rich native events and larger-than-command-limit payloads belong only to A.
    for prompt in ["show native output", "show large output"] {
        cmd_a
            .send(ClientCommand::Prompt {
                session_id: first.clone(),
                text: prompt.into(),
            })
            .unwrap();
        let mut output = String::new();
        let mut saw_future = false;
        loop {
            match event(&mut events_a, |_| true).await {
                ServerEvent::NativeEvent {
                    session_id,
                    method,
                    params,
                } => {
                    assert_eq!(session_id, first);
                    assert!(!params.to_string().contains("foreign-thread-marker"));
                    assert!(!params.to_string().contains("global-account-marker"));
                    if method == "item/commandExecution/outputDelta" {
                        output.push_str(params["delta"].as_str().unwrap());
                    }
                    if method == "fixture/futureNotification" {
                        saw_future = true;
                    }
                    if method == "thread/tokenUsage/updated" {
                        // The server monitor sees the same live snapshot before
                        // the client receives it, while this turn is still active.
                        let monitor_event = tokio::time::timeout(Duration::from_secs(5), async {
                            loop {
                                let event = monitor_rx.recv().await.unwrap();
                                monitor_events.push(event.clone());
                                if matches!(event, MonitorEvent::SessionUsage { .. }) {
                                    break event;
                                }
                            }
                        })
                        .await
                        .unwrap();
                        let MonitorEvent::SessionUsage {
                            session_id,
                            params: usage,
                        } = monitor_event
                        else {
                            unreachable!()
                        };
                        assert_eq!(session_id, first);
                        assert_eq!(usage, params);
                    }
                }
                ServerEvent::TurnFinished { status, .. } => {
                    assert_eq!(status, "completed");
                    break;
                }
                ServerEvent::Error { message, .. }
                | ServerEvent::PromptRejected { message, .. } => {
                    panic!("unexpected error: {message}")
                }
                _ => {}
            }
        }
        if prompt == "show native output" {
            assert_eq!(output, "native output\n");
            assert!(saw_future);
        } else {
            assert_eq!(output, "x".repeat(2 * 1024 * 1024));
        }
    }
    cmd_b
        .send(ClientCommand::Prompt {
            session_id: second.clone(),
            text: "finish after steering".into(),
        })
        .unwrap();
    loop {
        match event(&mut events_b, |_| true).await {
            ServerEvent::NativeEvent { session_id, .. } => {
                assert_eq!(session_id, second, "foreign session output leaked")
            }
            ServerEvent::TurnFinished { .. } => break,
            ServerEvent::Error { message, .. } | ServerEvent::PromptRejected { message, .. } => {
                panic!("unexpected error: {message}")
            }
            _ => {}
        }
    }
    // Leave B busy again to exercise its disconnect interruption below.
    cmd_b
        .send(ClientCommand::Prompt {
            session_id: second.clone(),
            text: "wait until interrupted".into(),
        })
        .unwrap();
    event(&mut events_b, |e| {
        matches!(e, ServerEvent::TurnStarted { .. })
    })
    .await;
    cmd_a
        .send(ClientCommand::CloseSession {
            session_id: first.clone(),
        })
        .unwrap();
    event(&mut events_a, |e| {
        matches!(e, ServerEvent::SessionClosed { .. })
    })
    .await;
    assert!(
        !server.is_finished(),
        "closing a session must leave the server running"
    );
    assert!(
        calls(temp.path())
            .iter()
            .any(|call| call["method"] == "thread/delete" && call["params"]["threadId"] == first),
        "SessionClosed must confirm deletion before server shutdown"
    );
    stop_b.cancel();
    tokio::time::timeout(Duration::from_secs(15), b)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    stop_a.cancel();
    tokio::time::timeout(Duration::from_secs(15), a)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    server_stop.cancel();
    tokio::time::timeout(Duration::from_secs(20), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let records = calls(temp.path());
    for id in [&first, &second] {
        assert!(
            records
                .iter()
                .any(|v| v["method"] == "thread/delete" && v["params"]["threadId"] == *id),
            "thread was not deleted: {id}"
        );
    }
    assert!(
        records
            .iter()
            .any(|v| v["method"] == "turn/interrupt" && v["params"]["threadId"] == second)
    );
    assert!(
        !records
            .iter()
            .any(|v| v["params"]["input"][0]["text"] == "must be rejected")
    );
    for v in records.iter().filter(|v| v["method"] == "turn/start") {
        assert_eq!(v["params"]["sandboxPolicy"]["type"], "workspaceWrite");
        assert_eq!(v["params"]["sandboxPolicy"]["networkAccess"], true);
        let workspace = if v["params"]["threadId"] == first {
            &first_workspace
        } else {
            assert_eq!(v["params"]["threadId"], second);
            &second_workspace
        };
        assert_eq!(v["params"]["cwd"], *workspace);
        assert_eq!(
            v["params"]["sandboxPolicy"]["writableRoots"],
            serde_json::json!([workspace])
        );
    }
    while let Ok(event) = monitor_rx.try_recv() {
        monitor_events.push(event);
    }
    let clients: Vec<_> = monitor_events
        .iter()
        .filter_map(|event| match event {
            MonitorEvent::ClientConnected { id, peer, hostname } => {
                assert!(peer.starts_with("127.0.0.1:"));
                assert_eq!(
                    hostname.as_deref(),
                    Some(gethostname::gethostname().to_str().unwrap())
                );
                Some(id)
            }
            _ => None,
        })
        .collect();
    assert_eq!(clients.len(), 2);
    assert_ne!(clients[0], clients[1]);
    for (expected_id, expected_workspace) in
        [(&first, &first_workspace), (&second, &second_workspace)]
    {
        let opened = monitor_events.iter().find(|event| matches!(event, MonitorEvent::SessionOpened { session_id, .. } if session_id == expected_id)).unwrap();
        let MonitorEvent::SessionOpened {
            client_id,
            workspace,
            model,
            ..
        } = opened
        else {
            unreachable!()
        };
        assert!(clients.contains(&client_id));
        assert_eq!(workspace, expected_workspace);
        assert_eq!(model.as_deref(), Some("fixture"));
    }
    let usage: Vec<_> = monitor_events
        .iter()
        .filter_map(|event| match event {
            MonitorEvent::SessionUsage { session_id, params } => {
                assert_eq!(
                    session_id, &first,
                    "Usage must stay attached to its owning session"
                );
                Some(
                    params["tokenUsage"]["total"]["inputTokens"]
                        .as_u64()
                        .unwrap(),
                )
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        usage,
        [15, 30],
        "Two connected clients must not duplicate monitor updates"
    );
}

#[tokio::test]
async fn failed_deletion_is_retried_on_server_restart() {
    let temp = tempfile::tempdir().unwrap();
    let config = || BridgeConfig {
        executable: env!("CARGO_BIN_EXE_fake-codex").into(),
        workspace: temp.path().into(),
        state_dir: temp.path().join("state"),
        model: None,
    };
    let bridge = CodexBridge::start(config()).await.unwrap();
    let id = bridge.new_session("Recovery").await.unwrap();
    std::fs::write(temp.path().join("fail-delete"), "yes").unwrap();
    assert!(bridge.close_session(&id).await.is_err());
    let _ = bridge.shutdown().await;
    drop(bridge);
    std::fs::remove_file(temp.path().join("fail-delete")).unwrap();
    let bridge = CodexBridge::start(config()).await.unwrap();
    bridge.shutdown().await.unwrap();
    assert!(
        calls(temp.path())
            .iter()
            .filter(|v| v["method"] == "thread/delete" && v["params"]["threadId"] == id)
            .count()
            >= 2
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn closing_client_session_deletes_immediately_and_server_accepts_another() {
    let temp = tempfile::tempdir().unwrap();
    let (monitor_tx, mut monitor_rx) = mpsc::unbounded_channel();
    let server_stop = CancellationToken::new();
    let server = tokio::spawn(run_server(
        ServerConfig {
            bind: "127.0.0.1:0".into(),
            workspace: temp.path().join("work"),
            codex_path: env!("CARGO_BIN_EXE_fake-codex").into(),
            state_dir: temp.path().join("state"),
            model: None,
        },
        monitor_tx,
        server_stop.clone(),
    ));
    let config = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let MonitorEvent::Ready {
                address,
                pairing_code,
                ..
            } = monitor_rx.recv().await.unwrap()
            {
                break ClientConfig {
                    address,
                    pairing_code,
                };
            }
        }
    })
    .await
    .unwrap();
    let (commands, commands_rx) = mpsc::unbounded_channel();
    let (events_tx, mut events) = mpsc::unbounded_channel();
    let client_stop = CancellationToken::new();
    let client = tokio::spawn(run_client(
        config,
        commands_rx,
        events_tx,
        client_stop.clone(),
    ));
    event(&mut events, |event| {
        matches!(event, ServerEvent::Welcome { .. })
    })
    .await;
    commands.send(ClientCommand::ListModels).unwrap();
    let ServerEvent::Models { models, .. } = event(&mut events, |event| {
        matches!(event, ServerEvent::Models { .. })
    })
    .await
    else {
        unreachable!()
    };
    assert_eq!(models.len(), 2);

    for index in 0..2 {
        commands
            .send(ClientCommand::NewSession {
                name: format!("Close then recreate {index}"),
                options: SessionOptions {
                    model: Some("fixture-fast".into()),
                    effort: Some("medium".into()),
                    workspace: Some("chosen/working-folder".into()),
                },
            })
            .unwrap();
        let ServerEvent::SessionStarted {
            session_id,
            workspace,
            model,
            effort,
            ..
        } = event(&mut events, |event| {
            assert!(
                !matches!(event, ServerEvent::NativeEvent { .. }),
                "native creation event arrived before SessionStarted"
            );
            matches!(event, ServerEvent::SessionStarted { .. })
        })
        .await
        else {
            unreachable!()
        };
        assert_eq!(model.as_deref(), Some("fixture-fast"));
        assert_eq!(effort.as_deref(), Some("medium"));
        let mut native_started = false;
        let mut native_status = false;
        while !native_started || !native_status {
            if let ServerEvent::NativeEvent {
                session_id: owner,
                method,
                params,
            } = event(&mut events, |_| true).await
            {
                assert_eq!(owner, session_id);
                if method == "thread/started" {
                    assert_eq!(params["thread"]["id"], session_id);
                    native_started = true;
                }
                if method == "thread/status/changed" {
                    assert_eq!(params["marker"], "creation-event");
                    native_status = true;
                }
            }
        }
        assert_eq!(
            Path::new(&workspace),
            temp.path()
                .join("work/chosen/working-folder")
                .canonicalize()
                .unwrap()
        );
        std::fs::write(Path::new(&workspace).join("keep.txt"), "retained").unwrap();
        commands
            .send(ClientCommand::Prompt {
                session_id: session_id.clone(),
                text: "wait until interrupted".into(),
            })
            .unwrap();
        event(&mut events, |event| {
            matches!(event, ServerEvent::TurnStarted { .. })
        })
        .await;
        commands
            .send(ClientCommand::CloseSession {
                session_id: session_id.clone(),
            })
            .unwrap();
        event(&mut events, |event| matches!(event, ServerEvent::SessionClosed {session_id:closed} if closed==&session_id)).await;
        assert!(!server.is_finished());
        assert!(!client.is_finished());
        assert!(Path::new(&workspace).join("keep.txt").is_file());
        let records = calls(&temp.path().join("work"));
        let interrupted = records
            .iter()
            .position(|call| {
                call["method"] == "turn/interrupt" && call["params"]["threadId"] == session_id
            })
            .unwrap();
        let deleted = records
            .iter()
            .position(|call| {
                call["method"] == "thread/delete" && call["params"]["threadId"] == session_id
            })
            .unwrap();
        assert!(interrupted < deleted);
        assert!(
            std::fs::read_dir(temp.path().join("state/owned-threads"))
                .unwrap()
                .next()
                .is_none()
        );
    }
    client_stop.cancel();
    tokio::time::timeout(Duration::from_secs(15), client)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    server_stop.cancel();
    tokio::time::timeout(Duration::from_secs(20), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stopping_server_cleans_up_a_still_connected_busy_client() {
    let temp = tempfile::tempdir().unwrap();
    let (monitor_tx, mut monitor_rx) = mpsc::unbounded_channel();
    let stop = CancellationToken::new();
    let server = tokio::spawn(run_server(
        ServerConfig {
            bind: "127.0.0.1:0".into(),
            workspace: temp.path().into(),
            codex_path: env!("CARGO_BIN_EXE_fake-codex").into(),
            state_dir: temp.path().join("state"),
            model: None,
        },
        monitor_tx,
        stop.clone(),
    ));
    let config = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let MonitorEvent::Ready {
                address,
                pairing_code,
                ..
            } = monitor_rx.recv().await.unwrap()
            {
                break ClientConfig {
                    address,
                    pairing_code,
                };
            }
        }
    })
    .await
    .unwrap();
    let (commands, commands_rx) = mpsc::unbounded_channel();
    let (events_tx, mut events) = mpsc::unbounded_channel();
    let client = tokio::spawn(run_client(
        config,
        commands_rx,
        events_tx,
        CancellationToken::new(),
    ));
    event(&mut events, |e| matches!(e, ServerEvent::Welcome { .. })).await;
    commands
        .send(ClientCommand::NewSession {
            name: "Shutdown".into(),
            options: Default::default(),
        })
        .unwrap();
    let ServerEvent::SessionStarted { session_id, .. } = event(&mut events, |e| {
        matches!(e, ServerEvent::SessionStarted { .. })
    })
    .await
    else {
        unreachable!()
    };
    commands
        .send(ClientCommand::Prompt {
            session_id: session_id.clone(),
            text: "wait until interrupted".into(),
        })
        .unwrap();
    event(&mut events, |e| {
        matches!(e, ServerEvent::TurnStarted { .. })
    })
    .await;
    stop.cancel();
    tokio::time::timeout(Duration::from_secs(20), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(10), client)
        .await
        .unwrap()
        .unwrap();
    let records = calls(temp.path());
    let interrupt = records
        .iter()
        .position(|v| v["method"] == "turn/interrupt" && v["params"]["threadId"] == session_id)
        .unwrap();
    let delete = records
        .iter()
        .position(|v| v["method"] == "thread/delete" && v["params"]["threadId"] == session_id)
        .unwrap();
    assert!(
        interrupt < delete,
        "active work must be interrupted before deletion"
    );
}
