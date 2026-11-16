use crate::{client::Client, history::Detail};
use anyhow::Result;
use codex_lan_core::protocol::{ClientCommand, SessionOptions};
use rmcp::{
    RoleServer, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, ContentBlock, Implementation, ProgressNotificationParam,
        ServerCapabilities, ServerConfig,
    },
    schemars::{self, JsonSchema},
    service::RequestContext,
    tool, tool_handler, tool_router,
};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Create {
    /// Short task name, displayed on the server with a Claude prefix.
    name: String,
    /// Existing server directory inside its workspace root. Absolute, or relative to the root.
    workspace: String,
    /// Native model ID from server_info. Omit to inherit the server default.
    model: Option<String>,
    /// Effort supported by the selected model. Omit to inherit its default.
    effort: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Delegate {
    name: String,
    /// Prepared directory on the server, inside its workspace root.
    workspace: String,
    /// Assignment, or a short instruction to read TASK.md. Put large deliverables in files.
    text: String,
    model: Option<String>,
    effort: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Wait {
    thread_id: String,
    /// Read more of an oversized final answer only when needed; default 0.
    #[serde(default)]
    offset: usize,
    /// Final-answer bytes, 256–24000; default 4000. Questions are returned intact.
    #[serde(default = "default_result_bytes")]
    max_bytes: usize,
}

fn default_result_bytes() -> usize {
    4000
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Info {
    /// Include the model/effort catalog only when choosing a non-default model.
    #[serde(default)]
    catalog: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ThreadId {
    /// ID returned by thread_create in this MCP connection.
    thread_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SendArgs {
    thread_id: String,
    /// Self-contained task or steering message. Codex does not inherit Claude's conversation.
    text: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Read {
    thread_id: String,
    /// Previous next_cursor; omit to replay retained output, including after changing detail.
    cursor: Option<String>,
    #[serde(default)]
    detail: Detail,
    /// Wait for new visible output, a question or completion. Maximum 30000 ms; default 0.
    #[serde(default)]
    wait_ms: u64,
    /// Maximum text bytes in this page, 256–24000; default 8000.
    #[serde(default = "default_page_bytes")]
    max_bytes: usize,
}

fn default_page_bytes() -> usize {
    8000
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Reply {
    thread_id: String,
    /// Request ID returned with needs_input.
    request_id: String,
    /// Native result object matching the question's method and parameters.
    result: Value,
}

#[derive(Clone)]
pub struct McpServer {
    client: Client,
    tool_router: ToolRouter<Self>,
}

fn result(value: Result<Value>) -> CallToolResult {
    match value {
        Ok(value) => CallToolResult::success(vec![ContentBlock::text(value.to_string())]),
        Err(error) => CallToolResult::error(vec![ContentBlock::text(
            json!({"error":format!("{error:#}")}).to_string(),
        )]),
    }
}

/// Heartbeats are protocol notifications, not model-facing output or inference.
async fn waiting(
    context: RequestContext<RoleServer>,
    future: impl std::future::Future<Output = Result<Value>>,
) -> CallToolResult {
    tokio::pin!(future);
    let token = context.meta.get_progress_token();
    let started = std::time::Instant::now();
    loop {
        tokio::select! {
            value = &mut future => return result(value),
            _ = context.ct.cancelled() => return result(Err(anyhow::anyhow!("Wait cancelled; work continues. Use thread_list/thread_wait, or thread_interrupt to stop it."))),
            _ = tokio::time::sleep(std::time::Duration::from_secs(20)), if token.is_some() => {
                let _ = context.peer.notify_progress(ProgressNotificationParam::new(token.clone().unwrap(), started.elapsed().as_secs_f64())).await;
            }
        }
    }
}

#[tool_router]
impl McpServer {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Discover the server workspace root. Request catalog only to choose model/effort. No inference.",
        annotations(read_only_hint = true)
    )]
    async fn server_info(&self, Parameters(args): Parameters<Info>) -> CallToolResult {
        result(self.client.metadata(args.catalog).await)
    }

    #[tool(
        description = "Compact list of this connection's live threads and last 8 delegation results. Recover after a cancelled call before retrying work.",
        annotations(read_only_hint = true)
    )]
    async fn thread_list(&self) -> CallToolResult {
        result(Ok(self.client.list()))
    }

    #[tool(
        description = "Preferred delegation: create in a prepared server directory, send assignment, wait locally, return only final answer/question/failure, and delete the finished native thread. No model polling. Full trace stays in the server inspector; files remain. Cancellation stops waiting, not work/cleanup; recover via thread_list/thread_wait. Defaults to 4000 reply bytes; next_offset signals more. Never repeat an uncertain submission.",
        annotations(destructive_hint = true, idempotent_hint = false)
    )]
    async fn delegate(
        &self,
        Parameters(args): Parameters<Delegate>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        waiting(
            context,
            self.client.delegate(
                args.name,
                SessionOptions {
                    model: args.model,
                    effort: args.effort,
                    workspace: Some(args.workspace),
                    require_existing_workspace: true,
                },
                args.text,
            ),
        )
        .await
    }

    #[tool(
        description = "Wait locally for final answer, question or failure; ignores all progress. Also retrieves the last 8 automatically closed delegation results. No periodic model calls needed. Use next_offset only if more of a long final answer is necessary. Cancelling the wait leaves work running.",
        annotations(read_only_hint = true)
    )]
    async fn thread_wait(
        &self,
        Parameters(args): Parameters<Wait>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        waiting(
            context,
            self.client
                .wait_result(&args.thread_id, args.offset, args.max_bytes),
        )
        .await
    }

    #[tool(
        description = "Manual lifecycle: create in a prepared server directory, without prompting. Prefer delegate for one-shot subtasks. Files must exist; no upload or Git checkout.",
        annotations(destructive_hint = false, idempotent_hint = false)
    )]
    async fn thread_create(&self, Parameters(args): Parameters<Create>) -> CallToolResult {
        result(
            self.client
                .create(
                    args.name,
                    SessionOptions {
                        model: args.model,
                        effort: args.effort,
                        workspace: Some(args.workspace),
                        require_existing_workspace: true,
                    },
                )
                .await,
        )
    }

    #[tool(
        description = "Start or steer an owned thread. Returns acceptance; use thread_wait for the final result. Do not resend accepted work. Prefer delegate for a new subtask.",
        annotations(destructive_hint = true, idempotent_hint = false)
    )]
    async fn thread_send(&self, Parameters(args): Parameters<SendArgs>) -> CallToolResult {
        result(
            async {
                self.client.owns(&args.thread_id)?;
                self.client
                    .command(ClientCommand::Prompt {
                        session_id: args.thread_id,
                        text: args.text,
                    })
                    .await
            }
            .await,
        )
    }

    #[tool(
        description = "Opt-in diagnostics only; use thread_wait for normal results and the server inspector for human monitoring. Reads retained replies/progress/tools/everything with next_cursor pagination. Raw output consumes Claude context. Partial records have sequence/offset/complete. For automatically closed delegations, use thread_wait.",
        annotations(read_only_hint = true)
    )]
    async fn thread_read(&self, Parameters(args): Parameters<Read>) -> CallToolResult {
        result(
            self.client
                .read(
                    &args.thread_id,
                    args.cursor.as_deref(),
                    args.detail,
                    args.wait_ms,
                    args.max_bytes,
                )
                .await,
        )
    }

    #[tool(
        description = "Answer a pending question. For delegated threads also waits for the next question/final result and automatic cleanup. requestUserInput result: {answers:{question_id:{answers:[text]}}}.",
        annotations(destructive_hint = true, idempotent_hint = false)
    )]
    async fn thread_reply(
        &self,
        Parameters(args): Parameters<Reply>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        waiting(context, async {
            self.client.owns(&args.thread_id)?;
            self.client
                .reply(&args.thread_id, args.request_id, args.result)
                .await
        })
        .await
    }

    #[tool(
        description = "Stop an owned thread's active turn. Delegations then close automatically; manual threads stay open. Files remain. Use thread_wait for the outcome.",
        annotations(destructive_hint = true)
    )]
    async fn thread_interrupt(&self, Parameters(args): Parameters<ThreadId>) -> CallToolResult {
        result(
            async {
                self.client.owns(&args.thread_id)?;
                self.client
                    .command(ClientCommand::Interrupt {
                        session_id: args.thread_id,
                    })
                    .await
            }
            .await,
        )
    }

    #[tool(
        description = "Permanently delete an owned native Codex thread, stopping active work. Returns only after the server confirms deletion. Files remain; this adapter releases retained output, so collect the result first. Other clients' threads are unaffected.",
        annotations(destructive_hint = true)
    )]
    async fn thread_close(&self, Parameters(args): Parameters<ThreadId>) -> CallToolResult {
        result(
            async {
                self.client.owns(&args.thread_id)?;
                self.client
                    .command(ClientCommand::CloseSession {
                        session_id: args.thread_id,
                    })
                    .await
            }
            .await,
        )
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("codex-lan-mcp", env!("CARGO_PKG_VERSION")))
            .with_instructions("Use delegate for self-contained Codex subtasks in prepared server directories. Rust handles waiting and cleanup; do not poll thread_read. Human progress stays in the server inspector. Supply concise context/TASK.md, request file deliverables and a brief final handoff. thread_reply answers questions and waits. Recover cancelled waits with thread_list/thread_wait; never blindly resubmit. Last 8 results stay in this adapter until exit. Files remain; disconnected native threads are cleaned up, not resumed. Manual thread_create/send/wait/close is available for multi-turn work.")
    }
}
