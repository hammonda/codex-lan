//! Creates and deletes one owned Codex thread. Optional flags test a live reply or file creation.
use codex_lan_core::bridge::{BridgeConfig, CodexBridge};
use codex_lan_core::protocol::ServerEvent;
use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let workspace = std::env::args_os()
        .nth(1)
        .map(std::path::PathBuf::from)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "usage: lifecycle_smoke <temporary-workspace> [--prompt | --workspace-prompt]"
            )
        })?;
    let live_prompt = std::env::args().any(|arg| arg == "--prompt");
    let workspace_prompt = std::env::args().any(|arg| arg == "--workspace-prompt");
    let bridge = CodexBridge::start(BridgeConfig {
        executable: "codex".into(),
        state_dir: workspace.join("bridge-state"),
        workspace,
        model: None,
    })
    .await?;
    let operation = async {
        let id = bridge.new_session("Codex LAN lifecycle smoke test").await?;
        println!("Created owned test thread {id}");
        let session_workspace = bridge.session_workspace(&id)?;
        println!("Session workspace: {}", session_workspace.display());
        if live_prompt || workspace_prompt {
            let mut events = bridge.subscribe();
            let prompt = if workspace_prompt {
                "Check your current working directory using a shell command, then create workspace-check.txt in that directory containing exactly CODEX_LAN_WORKSPACE_OK followed by a newline. Return the absolute file path. Do not read or write outside your current working directory."
            } else {
                "Reply with exactly CODEX_LAN_OK. Do not use tools, read files, or change files."
            };
            bridge.prompt(&id, prompt).await?;
            tokio::time::timeout(Duration::from_secs(120), async {
                loop {
                    match events.recv().await? {
                        ServerEvent::ItemCompleted {text,..} => println!("Codex reply: {text}"),
                        ServerEvent::TurnFinished {status,..} => {
                            anyhow::ensure!(status == "completed", "Live turn ended with status {status}");
                            break;
                        }
                        ServerEvent::Error {message,..} => anyhow::bail!("{message}"),
                        _ => {}
                    }
                }
                Ok::<_,anyhow::Error>(())
            }).await??;
        }
        bridge.close_session(&id).await?;
        println!("Confirmed thread/delete for {id}");
        if workspace_prompt {
            let contents = std::fs::read_to_string(session_workspace.join("workspace-check.txt"))?;
            anyhow::ensure!(contents == "CODEX_LAN_WORKSPACE_OK\n", "Unexpected workspace marker contents");
            println!("Confirmed workspace artifact survived native thread deletion");
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let cleanup = bridge.shutdown().await;
    operation?;
    cleanup
}
