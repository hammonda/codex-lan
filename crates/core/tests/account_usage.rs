#![cfg(feature = "test-support")]

mod support;

use codex_lan_core::{
    bridge::{BridgeConfig, CodexBridge},
    protocol::{ClientCommand, ServerEvent},
    transport::{ClientConfig, MonitorEvent, ServerConfig, run_client, run_server},
};
use std::{collections::HashSet, time::Duration};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

async fn event(
    rx: &mut mpsc::UnboundedReceiver<ServerEvent>,
    predicate: impl Fn(&ServerEvent) -> bool,
) -> ServerEvent {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = rx.recv().await.unwrap();
            if predicate(&event) {
                return event;
            }
        }
    })
    .await
    .expect("client event timed out")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn account_usage_arrives_without_threads_and_disconnect_deletes_busy_and_idle_threads() {
    let temp = tempfile::tempdir().unwrap();
    let (monitor_tx, mut monitor_rx) = mpsc::unbounded_channel();
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
    let (address, pairing) = tokio::time::timeout(Duration::from_secs(10), async {
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
    assert!(pairing.starts_with("codex-lan-v4:"));
    let connect = || {
        let (commands, rx) = mpsc::unbounded_channel();
        let (events, event_rx) = mpsc::unbounded_channel();
        let stop = CancellationToken::new();
        let task = tokio::spawn(run_client(
            ClientConfig {
                address: address.clone(),
                pairing_code: pairing.clone(),
            },
            rx,
            events,
            stop.clone(),
        ));
        (commands, event_rx, stop, task)
    };
    let (commands_a, mut events_a, stop_a, client_a) = connect();
    let (commands_b, mut events_b, stop_b, client_b) = connect();
    for events in [&mut events_a, &mut events_b] {
        event(events, |e| matches!(e, ServerEvent::Welcome { .. })).await;
        let ServerEvent::AccountUsage { usage } =
            event(events, |e| matches!(e, ServerEvent::AccountUsage { .. })).await
        else {
            unreachable!()
        };
        assert_eq!(usage.ordinary_usage_allowed, Some(true));
        assert_eq!(usage.limits.len(), 2);
        let codex = usage
            .limits
            .iter()
            .find(|limit| limit.id == "codex")
            .unwrap();
        assert_eq!(codex.secondary.as_ref().unwrap().used_percent, 20);
        assert_eq!(
            codex.secondary.as_ref().unwrap().window_minutes,
            Some(10080)
        );
        assert!(!serde_json::to_string(&usage).unwrap().contains("private"));
    }
    std::fs::write(temp.path().join("emit-usage-update"), "").unwrap();
    commands_a.send(ClientCommand::ListModels).unwrap();
    for events in [&mut events_a, &mut events_b] {
        let ServerEvent::AccountUsage { usage } = event(events, |e| matches!(e, ServerEvent::AccountUsage { usage } if usage.limits.iter().any(|limit| limit.secondary.as_ref().is_some_and(|window| window.used_percent == 35)))).await else { unreachable!() };
        let codex = usage
            .limits
            .iter()
            .find(|limit| limit.id == "codex")
            .unwrap();
        assert_eq!(codex.primary.as_ref().unwrap().used_percent, 10);
        assert_eq!(
            codex.secondary.as_ref().unwrap().window_minutes,
            Some(10080)
        );
        assert_eq!(codex.plan.as_deref(), Some("plus"));
    }
    let calls = || support::calls(temp.path());
    assert!(
        !calls()
            .iter()
            .any(|call| call["method"] == "thread/start" || call["method"] == "turn/start")
    );

    let mut owned = Vec::new();
    for name in ["busy", "idle"] {
        commands_a
            .send(ClientCommand::NewSession {
                name: name.into(),
                options: Default::default(),
            })
            .unwrap();
        let ServerEvent::SessionStarted { session_id, .. } = event(&mut events_a, |e| {
            matches!(e, ServerEvent::SessionStarted { .. })
        })
        .await
        else {
            unreachable!()
        };
        owned.push(session_id);
    }
    commands_a
        .send(ClientCommand::Prompt {
            session_id: owned[0].clone(),
            text: "wait until interrupted".into(),
        })
        .unwrap();
    event(&mut events_a, |e| {
        matches!(e, ServerEvent::TurnStarted { .. })
    })
    .await;
    commands_b
        .send(ClientCommand::NewSession {
            name: "other client".into(),
            options: Default::default(),
        })
        .unwrap();
    let ServerEvent::SessionStarted {
        session_id: other, ..
    } = event(&mut events_b, |e| {
        matches!(e, ServerEvent::SessionStarted { .. })
    })
    .await
    else {
        unreachable!()
    };

    // This is the same cancellation token path as the main window close control.
    stop_a.cancel();
    tokio::time::timeout(Duration::from_secs(15), client_a)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let mut deleted = HashSet::new();
    tokio::time::timeout(Duration::from_secs(15), async {
        while deleted.len() < 2 {
            if let MonitorEvent::SessionClosed { session_id } = monitor_rx.recv().await.unwrap() {
                deleted.insert(session_id);
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(deleted, owned.iter().cloned().collect());
    assert!(!server.is_finished());
    let records = calls();
    assert!(
        records.iter().any(
            |call| call["method"] == "turn/interrupt" && call["params"]["threadId"] == owned[0]
        )
    );
    for id in &owned {
        assert!(
            records
                .iter()
                .any(|call| call["method"] == "thread/delete" && call["params"]["threadId"] == *id)
        );
    }
    assert!(
        !records
            .iter()
            .any(|call| call["method"] == "thread/delete" && call["params"]["threadId"] == other)
    );
    commands_b
        .send(ClientCommand::Prompt {
            session_id: other.clone(),
            text: "still connected".into(),
        })
        .unwrap();
    event(&mut events_b, |e| {
        matches!(e, ServerEvent::TurnFinished { .. })
    })
    .await;
    stop_b.cancel();
    tokio::time::timeout(Duration::from_secs(15), client_b)
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

#[tokio::test]
async fn non_chatgpt_or_unsupported_usage_is_unavailable_without_exposing_native_error() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("rate-limits-unavailable"), "").unwrap();
    let bridge = CodexBridge::start(BridgeConfig {
        executable: env!("CARGO_BIN_EXE_fake-codex").into(),
        workspace: temp.path().into(),
        state_dir: temp.path().join("state"),
        model: None,
    })
    .await
    .unwrap();
    let usage = bridge.account_usage().await;
    assert!(usage.limits.is_empty());
    assert!(usage.unavailable_reason.is_some());
    assert!(!serde_json::to_string(&usage).unwrap().contains("private"));
    let id = bridge.new_session("Still usable").await.unwrap();
    bridge.close_session(&id).await.unwrap();
    bridge.shutdown().await.unwrap();
}
