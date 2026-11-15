#![cfg(feature = "test-support")]

use codex_lan_core::bridge::{BridgeConfig, CodexBridge};
use std::{path::Path, time::Duration};

fn calls(workspace: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(workspace.join("fixture-rpc.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

async fn wait_until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("condition was not reached");
}

#[tokio::test]
async fn cancelled_create_deletes_its_late_successful_thread() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("delay-create"), "yes").unwrap();
    let bridge = CodexBridge::start(BridgeConfig {
        executable: env!("CARGO_BIN_EXE_fake-codex").into(),
        workspace: temp.path().into(),
        state_dir: temp.path().join("state"),
        model: None,
    })
    .await
    .unwrap();

    let creator = bridge.clone();
    let request = tokio::spawn(async move { creator.new_session("Cancelled").await });
    wait_until(|| {
        calls(temp.path())
            .iter()
            .any(|call| call["method"] == "thread/start")
    })
    .await;
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    let error = bridge.new_session("Too early").await.unwrap_err();
    assert!(error.to_string().contains("still awaiting Codex"));

    // No caller received this thread ID. Its late response must nevertheless
    // trigger deletion, and successful deletion must remove its journal entry.
    wait_until(|| {
        calls(temp.path())
            .iter()
            .any(|call| call["method"] == "thread/delete")
            && std::fs::read_dir(temp.path().join("state/owned-threads"))
                .unwrap()
                .next()
                .is_none()
    })
    .await;
    std::fs::remove_file(temp.path().join("delay-create")).unwrap();
    let next = bridge.new_session("Next session").await.unwrap();
    bridge.close_session(&next).await.unwrap();
    bridge.shutdown().await.unwrap();
    assert_eq!(
        calls(temp.path())
            .iter()
            .filter(|call| call["method"] == "thread/start")
            .count(),
        2
    );
    assert_eq!(
        calls(temp.path())
            .iter()
            .filter(|call| call["method"] == "thread/delete")
            .count(),
        2
    );
}

#[tokio::test]
async fn shutdown_waits_for_a_cancelled_create_and_its_deletion() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("delay-create"), "yes").unwrap();
    let bridge = CodexBridge::start(BridgeConfig {
        executable: env!("CARGO_BIN_EXE_fake-codex").into(),
        workspace: temp.path().into(),
        state_dir: temp.path().join("state"),
        model: None,
    })
    .await
    .unwrap();
    let creator = bridge.clone();
    let request = tokio::spawn(async move { creator.new_session("Cancelled at shutdown").await });
    wait_until(|| {
        calls(temp.path())
            .iter()
            .any(|call| call["method"] == "thread/start")
    })
    .await;
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());

    // No delay here: shutdown races the delayed response and its cleanup task.
    tokio::time::timeout(Duration::from_secs(5), bridge.shutdown())
        .await
        .unwrap()
        .unwrap();
    assert!(
        calls(temp.path())
            .iter()
            .any(|call| call["method"] == "thread/delete")
    );
    assert!(
        std::fs::read_dir(temp.path().join("state/owned-threads"))
            .unwrap()
            .next()
            .is_none()
    );
}

#[tokio::test]
async fn shutdown_reports_a_late_create_that_cannot_be_deleted() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("delay-create"), "yes").unwrap();
    std::fs::write(temp.path().join("fail-delete"), "yes").unwrap();
    let bridge = CodexBridge::start(BridgeConfig {
        executable: env!("CARGO_BIN_EXE_fake-codex").into(),
        workspace: temp.path().into(),
        state_dir: temp.path().join("state"),
        model: None,
    })
    .await
    .unwrap();
    let creator = bridge.clone();
    let request = tokio::spawn(async move { creator.new_session("Failed late cleanup").await });
    wait_until(|| {
        calls(temp.path())
            .iter()
            .any(|call| call["method"] == "thread/start")
    })
    .await;
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());

    let error = tokio::time::timeout(Duration::from_secs(5), bridge.shutdown())
        .await
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains("cleanup records remain"));
    assert!(
        std::fs::read_dir(temp.path().join("state/owned-threads"))
            .unwrap()
            .next()
            .is_some()
    );
}

#[tokio::test]
async fn shutdown_reports_an_unknown_creation_when_codex_exits() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("exit-on-create"), "yes").unwrap();
    let bridge = CodexBridge::start(BridgeConfig {
        executable: env!("CARGO_BIN_EXE_fake-codex").into(),
        workspace: temp.path().into(),
        state_dir: temp.path().join("state"),
        model: None,
    })
    .await
    .unwrap();
    assert!(bridge.new_session("Unresolved").await.is_err());
    let error = tokio::time::timeout(Duration::from_secs(5), bridge.shutdown())
        .await
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains("unknown thread/start results"));
}
