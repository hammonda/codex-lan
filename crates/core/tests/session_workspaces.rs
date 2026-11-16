#![cfg(feature = "test-support")]

mod support;

use codex_lan_core::{
    bridge::{BridgeConfig, CodexBridge},
    protocol::{ServerEvent, SessionOptions},
};
use serde_json::json;
use std::{collections::HashSet, path::Path, time::Duration};
use tokio::sync::broadcast;

async fn event(
    events: &mut broadcast::Receiver<ServerEvent>,
    predicate: impl Fn(&ServerEvent) -> bool,
) -> ServerEvent {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let next = events.recv().await.expect("bridge event stream closed");
            if predicate(&next) {
                return next;
            }
        }
    })
    .await
    .expect("timed out waiting for bridge event")
}

#[tokio::test]
async fn sessions_use_separate_directories_and_keep_their_files_after_cleanup() {
    let temp = tempfile::tempdir().unwrap();
    let requested_root = temp.path().join("new/working-root");
    assert!(!requested_root.exists());
    let state_dir = temp.path().join("state");
    let bridge = CodexBridge::start(BridgeConfig {
        executable: env!("CARGO_BIN_EXE_fake-codex").into(),
        workspace: requested_root.clone(),
        state_dir: state_dir.clone(),
        model: None,
    })
    .await
    .unwrap();
    let root = requested_root.canonicalize().unwrap();
    let mut events = bridge.subscribe();
    let names = [
        "Repeated name",
        "Repeated name",
        "../../outside\\C:\\temp\\CON",
    ];
    let mut sessions = Vec::new();
    for name in names {
        let id = bridge.new_session(name).await.unwrap();
        let workspace = bridge.session_workspace(&id).unwrap();
        assert!(workspace.is_dir());
        assert_eq!(
            workspace.parent(),
            Some(root.join("codex-lan-threads").as_path())
        );
        assert_eq!(workspace.canonicalize().unwrap(), workspace);
        let basename = workspace.file_name().unwrap().to_str().unwrap();
        assert!(!basename.contains(['/', '\\', ':']));
        let ServerEvent::SessionStarted {
            workspace: announced,
            name: announced_name,
            ..
        } = event(
            &mut events,
            |e| matches!(e, ServerEvent::SessionStarted { session_id, .. } if session_id == &id),
        )
        .await
        else {
            unreachable!()
        };
        assert_eq!(Path::new(&announced), workspace);
        assert_eq!(announced_name, name);
        // Artifacts belong to the user even after their native chat is deleted.
        std::fs::write(workspace.join("download.pdf"), format!("artifact for {id}")).unwrap();
        sessions.push((id, workspace));
    }
    assert_eq!(
        sessions
            .iter()
            .map(|(_, path)| path)
            .collect::<HashSet<_>>()
            .len(),
        names.len()
    );
    assert!(!temp.path().join("outside").exists());

    // Follow-up turns must keep the same directory as their initial turn.
    for index in [0, 1, 0, 2] {
        let id = &sessions[index].0;
        bridge.prompt(id, "hello").await.unwrap();
        let finished = event(
            &mut events,
            |e| matches!(e, ServerEvent::TurnFinished { session_id, .. } if session_id == id),
        )
        .await;
        assert!(
            matches!(finished, ServerEvent::TurnFinished { status, .. } if status == "completed")
        );
    }

    bridge.close_session(&sessions[0].0).await.unwrap();
    assert!(bridge.session_workspace(&sessions[0].0).is_err());
    assert!(sessions[0].1.join("download.pdf").is_file());
    bridge.shutdown().await.unwrap();

    let records = support::calls(&root);
    let creations: Vec<_> = records
        .iter()
        .filter(|call| call["method"] == "thread/start")
        .collect();
    assert_eq!(creations.len(), sessions.len());
    for (creation, (_, workspace)) in creations.iter().zip(&sessions) {
        let params = &creation["params"];
        assert_eq!(params["cwd"], json!(workspace));
        assert_eq!(
            params["config"]["sandbox_workspace_write.writable_roots"],
            json!([workspace])
        );
        assert_eq!(
            params["config"]["sandbox_workspace_write.network_access"],
            true
        );
    }
    let turns: Vec<_> = records
        .iter()
        .filter(|call| call["method"] == "turn/start")
        .collect();
    assert_eq!(turns.len(), 4);
    for turn in turns {
        let params = &turn["params"];
        let (_, workspace) = sessions
            .iter()
            .find(|(id, _)| params["threadId"] == *id)
            .unwrap();
        assert_eq!(params["cwd"], json!(workspace));
        assert_eq!(params["sandboxPolicy"]["type"], "workspaceWrite");
        assert_eq!(params["sandboxPolicy"]["writableRoots"], json!([workspace]));
        assert_eq!(params["sandboxPolicy"]["networkAccess"], true);
    }
    for ((id, workspace), name) in sessions.iter().zip(names) {
        assert!(records.iter().any(|call| {
            call["method"] == "thread/name/set"
                && call["params"]["threadId"] == *id
                && call["params"]["name"] == format!("LAN / {name}")
        }));
        assert!(records.iter().any(|call| {
            call["method"] == "thread/delete" && call["params"]["threadId"] == *id
        }));
        assert_eq!(
            std::fs::read_to_string(workspace.join("download.pdf")).unwrap(),
            format!("artifact for {id}")
        );
    }
    assert!(
        std::fs::read_dir(state_dir.join("owned-threads"))
            .unwrap()
            .next()
            .is_none()
    );
}

#[tokio::test]
async fn prepared_context_outside_automatic_folders_preserves_workspace_boundaries() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("root");
    let bridge = CodexBridge::start(BridgeConfig {
        executable: env!("CARGO_BIN_EXE_fake-codex").into(),
        workspace: root.clone(),
        state_dir: temp.path().join("state"),
        model: None,
    })
    .await
    .unwrap();
    let prepared = root.join("project/context");
    std::fs::create_dir_all(&prepared).unwrap();
    let context = bridge
        .new_session_with_options(
            "Claude context",
            &SessionOptions {
                workspace: Some("project/context".into()),
                require_existing_workspace: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let desktop = bridge.new_session("Desktop").await.unwrap();
    let automatic = root.join("codex-lan-threads").canonicalize().unwrap();
    assert_eq!(
        bridge.session_workspace(&context).unwrap(),
        prepared.canonicalize().unwrap()
    );
    assert_eq!(
        bridge.session_workspace(&desktop).unwrap().parent(),
        Some(automatic.as_path())
    );
    let creations = support::calls(&root);
    for creation in creations
        .iter()
        .filter(|call| call["method"] == "thread/start")
    {
        // A wider selection root must not widen an individual thread's write access.
        assert_eq!(
            creation["params"]["config"]["sandbox_workspace_write.writable_roots"],
            json!([creation["params"]["cwd"]])
        );
        assert_ne!(
            creation["params"]["cwd"],
            json!(root.canonicalize().unwrap())
        );
    }
    bridge.close_session(&desktop).await.unwrap();
    let parent = bridge
        .new_session_with_options(
            "Use automatic parent",
            &SessionOptions {
                workspace: Some("codex-lan-threads".into()),
                require_existing_workspace: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let before = std::fs::read_dir(&automatic).unwrap().count();
    assert!(bridge.new_session("Must not overlap").await.is_err());
    assert_eq!(std::fs::read_dir(&automatic).unwrap().count(), before);
    bridge.close_session(&parent).await.unwrap();

    #[cfg(unix)]
    {
        let outside = temp.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::rename(&automatic, root.join("retained-sessions")).unwrap();
        std::os::unix::fs::symlink(&outside, &automatic).unwrap();
        assert!(bridge.new_session("Must not escape").await.is_err());
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
    }
    bridge.shutdown().await.unwrap();
}
