//! One live turn verifies model selection, tool output, steering, and immediate deletion.
use anyhow::{Context, Result, ensure};
use codex_lan_core::{
    bridge::{BridgeConfig, CodexBridge},
    protocol::{ServerEvent, SessionOptions},
};
use std::{path::PathBuf, time::Duration};

#[tokio::main]
async fn main() -> Result<()> {
    let root = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .context("usage: features_smoke <temporary-workspace-root>")?;
    let bridge = CodexBridge::start(BridgeConfig {
        executable: "codex".into(),
        state_dir: root.join("bridge-state"),
        workspace: root,
        model: None,
    })
    .await?;
    let operation = async {
        let models = bridge.list_models().await?;
        let model = models
            .iter()
            .find(|model| model.is_default)
            .or_else(|| models.first())
            .context("Codex returned no selectable models")?;
        let options = SessionOptions {
            model: Some(model.model.clone()),
            effort: model.default_effort.clone(),
            workspace: Some("selected-session-folder".into()),
            ..Default::default()
        };
        let mut events = bridge.subscribe();
        let id = bridge
            .new_session_with_options("Codex LAN feature smoke test", &options)
            .await?;
        let workspace = bridge.session_workspace(&id)?;
        println!("Created owned test thread {id}");
        println!("Model/effort: {:?}; cwd: {}", bridge.session_model_effort(&id)?, workspace.display());
        ensure!(
            !bridge.prompt(&id, "In your current working directory, run a shell command that prints NATIVE_OUTPUT_OK and then sleeps for 3 seconds. Afterwards create workspace-check.txt containing exactly CODEX_LAN_WORKSPACE_OK followed by a newline. Report the absolute path. Do not read or write outside your current working directory.").await?,
            "First prompt unexpectedly steered an existing turn"
        );
        let mut steered = false;
        let mut native_output = false;
        let mut native_count = 0usize;
        tokio::time::timeout(Duration::from_secs(180), async {
            loop {
                match events.recv().await? {
                    ServerEvent::NativeEvent { method, params, .. } => {
                        native_count += 1;
                        if method == "item/commandExecution/outputDelta"
                            && params["delta"].as_str().is_some_and(|text| text.contains("NATIVE_OUTPUT_OK"))
                        {
                            native_output = true;
                        }
                        if method == "item/completed"
                            && params["item"]["aggregatedOutput"].as_str().is_some_and(|text| text.contains("NATIVE_OUTPUT_OK"))
                        {
                            native_output = true;
                        }
                        if !steered && method == "item/started" && params["item"]["type"] == "commandExecution" {
                            steered = bridge.prompt(&id, "Also create steer-check.txt in this same working directory containing exactly CODEX_LAN_STEER_OK followed by a newline, and report its absolute path too.").await?;
                            ensure!(steered, "Follow-up did not steer the active turn");
                            println!("Confirmed turn/steer while native command was running");
                        }
                    }
                    ServerEvent::ItemCompleted { text, .. } => println!("Codex: {text}"),
                    ServerEvent::TurnFinished { status, .. } => {
                        ensure!(status == "completed", "Live turn ended with {status}");
                        break;
                    }
                    ServerEvent::Error { message, .. } => anyhow::bail!(message),
                    _ => {}
                }
            }
            Ok::<_, anyhow::Error>(())
        }).await??;
        ensure!(steered, "No command appeared to steer");
        ensure!(native_output, "Native command output was not received");
        bridge.close_session(&id).await?;
        println!("Confirmed thread/delete while the bridge remains running");
        ensure!(std::fs::read_to_string(workspace.join("workspace-check.txt"))? == "CODEX_LAN_WORKSPACE_OK\n", "Unexpected first marker");
        ensure!(std::fs::read_to_string(workspace.join("steer-check.txt"))? == "CODEX_LAN_STEER_OK\n", "Unexpected steering marker");
        println!("Verified retained artifacts and {native_count} native events");
        Ok::<_, anyhow::Error>(())
    }.await;
    let cleanup = bridge.shutdown().await;
    operation?;
    cleanup
}
