//! Local scheduling and final-result filtering. No inference, retries or model polling.
use super::*;
use codex_lan_core::protocol::SessionOptions;

pub(crate) const DEFAULT_RESULT_BYTES: usize = 4_000;
const HANDOFF: &str = "\n\n[Bridge handoff]\nUnless the assignment requires inline content, save detailed deliverables in the workspace. Keep the final reply brief: outcome, file paths, validation and unresolved blockers. Do not repeat logs or file contents.";

impl Client {
    pub(crate) async fn metadata(&self, catalog: bool) -> Result<Value> {
        self.ready().await?;
        if catalog {
            let info = self.info().await?;
            Ok(json!({"server":info["server"],"catalog":info["catalog"]}))
        } else {
            let state = self.0.state.lock().unwrap();
            Ok(
                json!({"workspace_root":state.welcome.as_ref().and_then(|v| v.get("workspace_root")),"max_threads":8}),
            )
        }
    }

    pub(crate) async fn create(&self, name: String, options: SessionOptions) -> Result<Value> {
        ensure!(
            !name.trim().is_empty() && name.trim().len() <= 150,
            "Task name must contain 1–150 bytes"
        );
        ensure!(
            options
                .workspace
                .as_ref()
                .is_some_and(|p| !p.trim().is_empty()),
            "Prepared workspace is required"
        );
        let value = self
            .command(ClientCommand::NewSession {
                name: format!("Claude / {}", name.trim()),
                options,
            })
            .await?;
        Ok(
            json!({"thread_id":value["session_id"],"name":value["name"],"workspace":value["workspace"],"model":value["model"],"effort":value["effort"]}),
        )
    }

    /// Detaching the worker makes cancellation of a tool response safe even during creation.
    /// The task is still owned, discoverable through list, and automatically cleaned up.
    pub(crate) async fn delegate(
        &self,
        name: String,
        options: SessionOptions,
        text: String,
    ) -> Result<Value> {
        ensure!(
            !text.trim().is_empty() && text.len() + HANDOFF.len() <= 256 * 1024,
            "Prompt must be nonempty and fit within 256 KiB including the bridge handoff instruction"
        );
        let text = format!("{text}{HANDOFF}");
        let client = self.clone();
        tokio::spawn(async move {
            let created = client.create(name, options).await?;
            let id = created["thread_id"].as_str().context("Missing created thread ID")?.to_owned();
            match client.command(ClientCommand::Prompt { session_id: id.clone(), text }).await {
                Ok(_) => client.manage(&id),
                Err(error) => {
                    client.finish(&id, json!({"thread_id":id,"status":"submission_failed","error":format!("{error:#}")})).await;
                }
            }
            client.wait_result(&id, 0, DEFAULT_RESULT_BYTES).await
        }).await.context("Delegation worker stopped")?
    }

    pub(crate) async fn reply(&self, id: &str, request_id: String, result: Value) -> Result<Value> {
        let managed = self.0.state.lock().unwrap().managed.contains(id);
        let ack = self
            .command(ClientCommand::Reply {
                session_id: id.into(),
                request_id: request_id.clone(),
                result,
            })
            .await?;
        // The command acknowledgement can overtake the broadcast RequestResolved.
        // A successful reply already resolves this request; never ask it twice.
        if let Some(thread) = self.0.state.lock().unwrap().threads.get_mut(id) {
            thread.requests.remove(&request_id);
        }
        self.0.changed.send_replace(());
        if managed {
            self.wait_result(id, 0, DEFAULT_RESULT_BYTES).await
        } else {
            Ok(ack)
        }
    }

    fn manage(&self, id: &str) {
        self.0.state.lock().unwrap().managed.insert(id.into());
        let client = self.clone();
        let id = id.to_owned();
        tokio::spawn(async move {
            let mut changed = client.0.changed.subscribe();
            loop {
                let result = {
                    let state = client.0.state.lock().unwrap();
                    match state.threads.get(&id) {
                        Some(thread) if thread.active() => None,
                        Some(thread) => Some(thread.outcome()),
                        None => Some(json!({"thread_id":id,"status":"closed","closed":true})),
                    }
                };
                if let Some(result) = result {
                    client.finish(&id, result).await;
                    return;
                }
                tokio::select! {
                    _ = client.0.stop.cancelled() => return,
                    result = changed.changed() => if result.is_err() { return; },
                }
            }
        });
    }

    async fn finish(&self, id: &str, mut result: Value) {
        {
            let mut state = self.0.state.lock().unwrap();
            state.closing.insert(id.into());
            if let Some(thread) = state.threads.get(id) {
                result["name"] = thread.brief()["name"].clone();
            }
        }
        if result["status"] == "disconnected" {
            result["closed"] = json!(false);
            result["cleanup_error"] = json!(
                "Connection ended. The server owns cleanup; deletion is unconfirmed. Do not replay the task."
            );
        } else if result["closed"] != true {
            match self
                .command(ClientCommand::CloseSession {
                    session_id: id.into(),
                })
                .await
            {
                Ok(_) => result["closed"] = json!(true),
                Err(error) => {
                    result["closed"] = json!(false);
                    result["cleanup_error"] = json!(format!("{error:#}"));
                }
            }
        }
        {
            let mut state = self.0.state.lock().unwrap();
            state.completed.push_back((id.into(), result));
            while state.completed.len() > 8 {
                state.completed.pop_front();
            }
            state.managed.remove(id);
            state.closing.remove(id);
        }
        self.0.changed.send_replace(());
    }

    /// Wait on local events, ignoring commentary, deltas, usage and tool output.
    /// No time-based responses: only completion, input or failure wakes the caller.
    pub(crate) async fn wait_result(
        &self,
        id: &str,
        offset: usize,
        max_bytes: usize,
    ) -> Result<Value> {
        ensure!(
            (256..=24_000).contains(&max_bytes),
            "max_bytes must be between 256 and 24000"
        );
        let mut changed = self.0.changed.subscribe();
        loop {
            {
                let state = self.0.state.lock().unwrap();
                if let Some((_, result)) = state.completed.iter().find(|(key, _)| key == id) {
                    return result_page(result.clone(), offset, max_bytes);
                }
                if !state.closing.contains(id) {
                    let thread = state
                        .threads
                        .get(id)
                        .context("Unknown thread or expired result; inspect thread_list")?;
                    if thread.current_status() == "needs_input"
                        || (!thread.active() && !state.managed.contains(id))
                    {
                        return result_page(thread.outcome(), offset, max_bytes);
                    }
                }
            }
            tokio::select! {
                _ = self.0.stop.cancelled() => bail!("MCP connection is shutting down"),
                result = changed.changed() => result.context("LAN worker stopped")?,
            }
        }
    }
}

fn result_page(mut result: Value, offset: usize, max_bytes: usize) -> Result<Value> {
    if let Some(text) = result["reply"].as_str() {
        ensure!(
            offset <= text.len() && text.is_char_boundary(offset),
            "Invalid reply offset"
        );
        let mut end = offset.saturating_add(max_bytes).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let page = text[offset..end].to_owned();
        let total = text.len();
        if offset != 0 || end < total {
            result["reply_bytes"] = json!(total);
            result["offset"] = json!(offset);
        }
        if end < total {
            result["next_offset"] = json!(end);
        }
        result["reply"] = json!(page);
    } else {
        ensure!(offset == 0, "No reply to read at this offset");
    }
    Ok(result)
}
