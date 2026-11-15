#![cfg(feature = "test-support")]

mod support;

use codex_lan_core::{
    bridge::{BridgeConfig, CodexBridge},
    protocol::{ServerEvent, SessionOptions},
};
use serde_json::json;
use std::{path::Path, time::Duration};
use support::calls;
use tokio::sync::broadcast;

async fn bridge(root: &Path, state: &Path) -> CodexBridge {
    CodexBridge::start(BridgeConfig {
        executable: env!("CARGO_BIN_EXE_fake-codex").into(),
        workspace: root.into(),
        state_dir: state.into(),
        model: None,
    })
    .await
    .unwrap()
}

async fn until_finished(events: &mut broadcast::Receiver<ServerEvent>) -> Vec<ServerEvent> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut received = Vec::new();
        loop {
            let event = events.recv().await.unwrap();
            let finished = matches!(event, ServerEvent::TurnFinished { .. });
            received.push(event);
            if finished {
                return received;
            }
        }
    })
    .await
    .expect("turn did not finish")
}

#[tokio::test]
async fn catalog_choices_and_effort_reach_native_creation_and_followup_turns() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("root");
    let bridge = bridge(&root, &temp.path().join("state")).await;
    let models = bridge.list_models().await.unwrap();
    assert_eq!(models.len(), 2, "catalog pagination must be followed");
    let default = models.iter().find(|model| model.is_default).unwrap();
    assert_eq!(default.model, "fixture");
    assert_eq!(default.default_effort.as_deref(), Some("medium"));
    assert_eq!(default.efforts, ["low", "medium", "high"]);
    assert!(models.iter().any(|model| model.model == "fixture-fast"));

    for options in [
        SessionOptions {
            model: Some("unavailable".into()),
            ..Default::default()
        },
        SessionOptions {
            model: Some("fixture-fast".into()),
            effort: Some("high".into()),
            ..Default::default()
        },
        SessionOptions {
            effort: Some("medium".into()),
            ..Default::default()
        },
    ] {
        assert!(
            bridge
                .new_session_with_options("Invalid settings", &options)
                .await
                .is_err()
        );
    }
    assert!(
        !calls(&root)
            .iter()
            .any(|call| call["method"] == "thread/start")
    );

    let mut events = bridge.subscribe();
    let id = bridge
        .new_session_with_options(
            "Custom settings",
            &SessionOptions {
                model: Some("fixture-fast".into()),
                effort: Some("medium".into()),
                workspace: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        bridge.session_model_effort(&id).unwrap(),
        (Some("fixture-fast".into()), Some("medium".into()))
    );
    for _ in 0..2 {
        assert!(!bridge.prompt(&id, "hello").await.unwrap());
        until_finished(&mut events).await;
    }
    bridge.close_session(&id).await.unwrap();
    bridge.shutdown().await.unwrap();
    let records = calls(&root);
    let creation = records
        .iter()
        .find(|call| call["method"] == "thread/start")
        .unwrap();
    assert_eq!(creation["params"]["model"], "fixture-fast");
    assert_eq!(
        creation["params"]["config"]["model_reasoning_effort"],
        "medium"
    );
    let turns: Vec<_> = records
        .iter()
        .filter(|call| call["method"] == "turn/start")
        .collect();
    assert_eq!(turns.len(), 2);
    for turn in turns {
        assert_eq!(turn["params"]["model"], "fixture-fast");
        assert_eq!(turn["params"]["effort"], "medium");
    }
    let configured = CodexBridge::start(BridgeConfig {
        executable: env!("CARGO_BIN_EXE_fake-codex").into(),
        workspace: root.clone(),
        state_dir: temp.path().join("configured-state"),
        model: Some("fixture-fast".into()),
    })
    .await
    .unwrap();
    let id = configured
        .new_session_with_options(
            "Server model with client effort",
            &SessionOptions {
                effort: Some("medium".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        configured.session_model_effort(&id).unwrap(),
        (Some("fixture-fast".into()), Some("medium".into()))
    );
    configured.close_session(&id).await.unwrap();
    configured.shutdown().await.unwrap();
}

#[tokio::test]
async fn creation_notifications_wait_for_publication_and_keep_their_native_fields() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("root");
    let bridge = bridge(&root, &temp.path().join("state")).await;
    let mut events = bridge.subscribe();
    let id = bridge
        .new_session("Early native notifications")
        .await
        .unwrap();
    let started = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(started, ServerEvent::SessionStarted {ref session_id, ..} if session_id == &id)
    );
    while let Ok(event) = events.try_recv() {
        assert!(
            !matches!(event, ServerEvent::NativeEvent { .. }),
            "native events escaped before publication"
        );
    }
    bridge.publish_session(&id).unwrap();
    let mut started = false;
    let mut status = false;
    tokio::time::timeout(Duration::from_secs(5), async {
        while !started || !status {
            if let ServerEvent::NativeEvent {
                session_id,
                method,
                params,
            } = events.recv().await.unwrap()
            {
                assert_eq!(session_id, id);
                if method == "thread/started" {
                    assert_eq!(params["thread"]["id"], id);
                    assert_eq!(
                        params["thread"]["cwd"],
                        json!(bridge.session_workspace(&id).unwrap())
                    );
                    started = true;
                }
                if method == "thread/status/changed" {
                    assert_eq!(params["marker"], "creation-event");
                    status = true;
                }
            }
        }
    })
    .await
    .expect("early native notifications were lost");
    bridge.close_session(&id).await.unwrap();
    bridge.shutdown().await.unwrap();
}

#[tokio::test]
async fn active_prompts_steer_and_only_explicit_completed_race_starts_a_new_turn() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("root");
    let bridge = bridge(&root, &temp.path().join("state")).await;
    let id = bridge.new_session("Steering").await.unwrap();
    let mut events = bridge.subscribe();
    assert!(!bridge.prompt(&id, "wait until interrupted").await.unwrap());
    let (first, second) = tokio::join!(
        bridge.prompt(&id, "First direction"),
        bridge.prompt(&id, "Second direction")
    );
    assert!(first.unwrap());
    assert!(second.unwrap());
    assert!(bridge.prompt(&id, "reject steering").await.is_err());
    assert!(bridge.prompt(&id, "finish after steering").await.unwrap());
    until_finished(&mut events).await;
    assert!(!bridge.prompt(&id, "wait until interrupted").await.unwrap());
    assert!(!bridge.prompt(&id, "complete while steering").await.unwrap());
    // The fixture completes the old turn before rejecting steering, then completes
    // the new turn. Closing still has to handle either notification ordering.
    bridge.close_session(&id).await.unwrap();
    bridge.shutdown().await.unwrap();
    let records = calls(&root);
    let starts: Vec<_> = records
        .iter()
        .filter(|call| call["method"] == "turn/start")
        .collect();
    assert_eq!(
        starts.len(),
        3,
        "successful or unrelated rejected steering must not start turns"
    );
    assert_eq!(
        starts[2]["params"]["input"][0]["text"],
        "complete while steering"
    );
    let steers: Vec<_> = records
        .iter()
        .filter(|call| call["method"] == "turn/steer")
        .collect();
    assert_eq!(steers.len(), 5);
    let first_turn = steers[0]["params"]["expectedTurnId"].as_str().unwrap();
    assert!(!first_turn.is_empty());
    for steer in &steers[..4] {
        assert_eq!(steer["params"]["expectedTurnId"], first_turn);
        assert_eq!(steer["params"]["threadId"], id);
        assert!(steer["params"].get("cwd").is_none());
        assert!(steer["params"].get("effort").is_none());
    }
    assert_ne!(steers[4]["params"]["expectedTurnId"], first_turn);
}

#[tokio::test]
async fn emitted_native_details_are_preserved_without_global_or_foreign_events() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("root");
    let bridge = bridge(&root, &temp.path().join("state")).await;
    let id = bridge.new_session("Full native output").await.unwrap();
    let mut events = bridge.subscribe();
    bridge.prompt(&id, "show native output").await.unwrap();
    let received = until_finished(&mut events).await;
    let native: Vec<_> = received
        .iter()
        .filter_map(|event| match event {
            ServerEvent::NativeEvent {
                session_id,
                method,
                params,
            } => {
                assert_eq!(session_id, &id);
                Some((method.as_str(), params))
            }
            _ => None,
        })
        .collect();
    for method in [
        "item/started",
        "item/commandExecution/outputDelta",
        "item/completed",
        "turn/plan/updated",
        "turn/diff/updated",
        "item/reasoning/summaryTextDelta",
        "thread/tokenUsage/updated",
        "fixture/futureNotification",
    ] {
        assert!(
            native.iter().any(|(actual, _)| *actual == method),
            "missing native event {method}"
        );
    }
    let output: String = native
        .iter()
        .filter(|(method, _)| *method == "item/commandExecution/outputDelta")
        .map(|(_, params)| params["delta"].as_str().unwrap())
        .collect();
    assert_eq!(output, "native output\n");
    let unknown = native
        .iter()
        .find(|(method, _)| *method == "fixture/futureNotification")
        .unwrap()
        .1;
    assert_eq!(unknown["detail"], json!({"preserve":"all emitted fields"}));
    assert!(!native.iter().any(
        |(_, params)| params.to_string().contains("foreign-thread-marker")
            || params.to_string().contains("global-account-marker")
    ));
    bridge.close_session(&id).await.unwrap();
    bridge.shutdown().await.unwrap();
}

#[tokio::test]
async fn custom_directories_stay_inside_root_without_traversal_or_symlink_side_effects() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("root");
    let state = temp.path().join("state");
    let bridge = bridge(&root, &state).await;
    let nested = SessionOptions {
        workspace: Some("projects/optics".into()),
        ..Default::default()
    };
    let first = bridge
        .new_session_with_options("Nested", &nested)
        .await
        .unwrap();
    assert_eq!(
        bridge.session_workspace(&first).unwrap(),
        root.join("projects/optics").canonicalize().unwrap()
    );
    std::fs::write(root.join("projects/optics/retained.txt"), "keep").unwrap();
    assert!(
        bridge
            .new_session_with_options("Overlap", &nested)
            .await
            .is_err()
    );
    bridge.close_session(&first).await.unwrap();
    let absolute = SessionOptions {
        workspace: Some(root.join("projects/optics").display().to_string()),
        ..Default::default()
    };
    let second = bridge
        .new_session_with_options("Reuse", &absolute)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(
            bridge
                .session_workspace(&second)
                .unwrap()
                .join("retained.txt")
        )
        .unwrap(),
        "keep"
    );
    bridge.close_session(&second).await.unwrap();

    for workspace in [
        "../outside".to_owned(),
        "new/../../outside".to_owned(),
        "C:\\outside\\child".to_owned(),
        root.display().to_string(),
        state.join("attempted").display().to_string(),
    ] {
        assert!(
            bridge
                .new_session_with_options(
                    "Rejected path",
                    &SessionOptions {
                        workspace: Some(workspace.clone()),
                        ..Default::default()
                    }
                )
                .await
                .is_err(),
            "accepted unsafe path: {workspace}"
        );
    }
    assert!(!temp.path().join("outside").exists());
    assert!(
        !root.join("new").exists(),
        "must reject traversal before creating directories"
    );
    assert!(!state.join("attempted").exists());
    #[cfg(unix)]
    {
        let outside = temp.path().join("outside-target");
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("shortcut")).unwrap();
        assert!(
            bridge
                .new_session_with_options(
                    "Symlink escape",
                    &SessionOptions {
                        workspace: Some("shortcut/created-outside".into()),
                        ..Default::default()
                    }
                )
                .await
                .is_err()
        );
        assert!(!outside.join("created-outside").exists());
    }
    bridge.shutdown().await.unwrap();
    assert!(root.join("projects/optics/retained.txt").exists());
}
