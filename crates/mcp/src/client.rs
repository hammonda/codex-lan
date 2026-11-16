use crate::history::{Detail, Thread};
use anyhow::{Context, Result, anyhow, bail, ensure};
use codex_lan_core::{
    protocol::{ClientCommand, ClientRequest, MAX_MESSAGE_BYTES, ServerEvent},
    transport::{ClientConfig, run_client_requests},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
    time::{Instant, timeout, timeout_at},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

mod delegation;

#[derive(Default)]
struct State {
    commands: Option<mpsc::UnboundedSender<ClientRequest>>,
    welcome: Option<Value>,
    models: Option<Value>,
    allowance: Option<Value>,
    ended: Option<String>,
    errors: VecDeque<String>,
    threads: BTreeMap<String, Thread>,
    retired: VecDeque<String>,
    managed: std::collections::HashSet<String>,
    closing: std::collections::HashSet<String>,
    completed: VecDeque<(String, Value)>,
}

struct PendingCommand {
    reply: oneshot::Sender<Result<Value, String>>,
    prompt_thread: Option<String>,
}
type Pending = HashMap<String, PendingCommand>;

struct Inner {
    reconnect: bool,
    state: Mutex<State>,
    pending: Mutex<Pending>,
    changed: watch::Sender<()>,
    stop: CancellationToken,
    task: Mutex<Option<JoinHandle<()>>>,
}

#[derive(Clone)]
pub struct Client(Arc<Inner>);

enum Source {
    Fixed(ClientConfig),
    File(PathBuf),
}

impl Source {
    fn config(&self) -> Result<ClientConfig> {
        match self {
            Self::Fixed(config) => Ok(config.clone()),
            Self::File(path) => {
                #[derive(serde::Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Config {
                    address: String,
                    pairing_code: String,
                }
                let bytes =
                    std::fs::read(path).with_context(|| format!("Read {}", path.display()))?;
                ensure!(
                    bytes.len() <= 128 * 1024,
                    "Connection configuration is too large"
                );
                let config: Config = serde_json::from_slice(&bytes).context(
                    "Invalid connection configuration (expected address and pairing_code)",
                )?;
                ensure!(
                    !config.address.trim().is_empty() && !config.pairing_code.trim().is_empty(),
                    "Address and pairing_code must not be empty"
                );
                Ok(ClientConfig {
                    address: config.address,
                    pairing_code: config.pairing_code,
                })
            }
        }
    }
}

impl Client {
    pub fn start(config: ClientConfig) -> Self {
        Self::spawn(Source::Fixed(config))
    }

    /// Re-read local pairing credentials between connections. Never replay a command or resume a thread.
    pub fn from_config_file(path: PathBuf) -> Result<Self> {
        let source = Source::File(path);
        source.config()?;
        Ok(Self::spawn(source))
    }

    fn spawn(source: Source) -> Self {
        let (changed, _) = watch::channel(());
        let inner = Arc::new(Inner {
            reconnect: matches!(source, Source::File(_)),
            changed,
            state: Mutex::new(State::default()),
            pending: Mutex::new(HashMap::new()),
            stop: CancellationToken::new(),
            task: Mutex::new(None),
        });
        let shared = inner.clone();
        let task = tokio::spawn(async move {
            while !shared.stop.is_cancelled() {
                let config = match source.config() {
                    Ok(config) => config,
                    Err(error) => {
                        shared
                            .disconnected(format!("Cannot load pairing configuration: {error:#}"));
                        tokio::select! { _ = shared.stop.cancelled() => break, _ = tokio::time::sleep(Duration::from_secs(1)) => {} }
                        continue;
                    }
                };
                let (commands, command_rx) = mpsc::unbounded_channel();
                let (event_tx, mut events) = mpsc::unbounded_channel();
                shared.state.lock().unwrap().commands = Some(commands);
                let mut connection = tokio::spawn(run_client_requests(
                    config,
                    command_rx,
                    event_tx,
                    shared.stop.clone(),
                ));
                let ended = loop {
                    tokio::select! {
                        event = events.recv() => {
                            match event {
                                Some(event) => shared.event(event),
                                None => {
                                    break match connection.await {
                                        Ok(Ok(())) => "LAN connection closed; its threads cannot be resumed".into(),
                                        Ok(Err(error)) => format!("LAN connection ended: {error:#}; its threads cannot be resumed"),
                                        Err(error) => format!("LAN worker stopped: {error}"),
                                    };
                                }
                            }
                        }
                        result = &mut connection => {
                            // Deliver acknowledgements already received before reporting disconnection.
                            while let Ok(event) = events.try_recv() { shared.event(event); }
                            break match result {
                                Ok(Ok(())) => "LAN connection closed; its threads cannot be resumed".into(),
                                Ok(Err(error)) => format!("LAN connection ended: {error:#}; its threads cannot be resumed"),
                                Err(error) => format!("LAN worker stopped: {error}"),
                            };
                        }
                    }
                };
                shared.disconnected(ended);
                if !shared.reconnect {
                    break;
                }
                tokio::select! { _ = shared.stop.cancelled() => break, _ = tokio::time::sleep(Duration::from_secs(1)) => {} }
            }
        });
        *inner.task.lock().unwrap() = Some(task);
        Self(inner)
    }

    pub async fn shutdown(&self) {
        self.0.stop.cancel();
        let task = self.0.task.lock().unwrap().take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }

    async fn ready(&self) -> Result<()> {
        let mut changed = self.0.changed.subscribe();
        timeout(Duration::from_secs(15), async {
            loop {
                {
                    let state = self.0.state.lock().unwrap();
                    if let Some(ended) = &state.ended
                        && !self.0.reconnect
                    {
                        bail!("{ended}");
                    }
                    if state.welcome.is_some() {
                        return Ok(());
                    }
                }
                changed.changed().await.context("LAN worker stopped")?;
            }
        })
        .await
        .context("Timed out connecting to the LAN server")?
    }

    pub(crate) async fn command(&self, command: ClientCommand) -> Result<Value> {
        self.ready().await?;
        let request = ClientRequest {
            command_id: Some(Uuid::new_v4().to_string()),
            command,
        };
        ensure!(
            serde_json::to_vec(&request)?.len() <= MAX_MESSAGE_BYTES,
            "Command is too large (maximum 1 MiB)"
        );
        let id = request.command_id.clone().unwrap();
        let (tx, rx) = oneshot::channel();
        let prompt_thread = match &request.command {
            ClientCommand::Prompt { session_id, .. } => Some(session_id.clone()),
            _ => None,
        };
        let sender = {
            let mut pending = self.0.pending.lock().unwrap();
            ensure!(
                pending.len() < 16,
                "Too many pending commands; wait for one to finish"
            );
            let mut state = self.0.state.lock().unwrap();
            ensure!(
                state.welcome.is_some(),
                "LAN connection changed before submission; inspect thread_list before retrying"
            );
            let sender = state.commands.clone().context("LAN connection closed")?;
            let owned_thread = match &request.command {
                ClientCommand::Prompt { session_id, .. }
                | ClientCommand::Reply { session_id, .. }
                | ClientCommand::Interrupt { session_id }
                | ClientCommand::CloseSession { session_id } => Some(session_id),
                _ => None,
            };
            if let Some(thread_id) = owned_thread {
                let thread = state.threads.get(thread_id).context("Unknown thread")?;
                ensure!(
                    thread.status != "disconnected",
                    "This thread's connection ended; create a new thread. Old prompts are never resent."
                );
            }
            if let Some(thread_id) = &prompt_thread {
                ensure!(
                    !state.closing.contains(thread_id),
                    "Delegated thread is finishing; start a new task"
                );
                if state.managed.contains(thread_id) {
                    ensure!(
                        state.threads.get(thread_id).is_some_and(Thread::active),
                        "Finished delegations close automatically; start a new task"
                    );
                }
                let thread = state
                    .threads
                    .get_mut(thread_id)
                    .context("Thread closed before submission")?;
                thread.submissions.insert(id.clone());
            }
            pending.insert(
                id.clone(),
                PendingCommand {
                    reply: tx,
                    prompt_thread: prompt_thread.clone(),
                },
            );
            sender
        };
        if sender.send(request).is_err() {
            self.0.pending.lock().unwrap().remove(&id);
            if let Some(thread_id) = prompt_thread
                && let Some(thread) = self.0.state.lock().unwrap().threads.get_mut(&thread_id)
            {
                thread.submissions.remove(&id);
            }
            bail!("LAN connection closed");
        }
        // Cancellation of an MCP call does not cancel a sent mutation. The event pump
        // still records its result, including a late-created thread, for list/cleanup.
        timeout(Duration::from_secs(120), rx).await
            .context("Command timed out; it may have completed. Inspect thread_list/thread_read before retrying")?
            .context("LAN connection closed before acknowledgement")?
            .map_err(|error| anyhow!(error))
    }

    pub async fn info(&self) -> Result<Value> {
        self.ready().await?;
        if self.0.state.lock().unwrap().models.is_none() {
            self.command(ClientCommand::ListModels).await?;
        }
        let state = self.0.state.lock().unwrap();
        Ok(
            json!({"server":state.welcome,"catalog":state.models,"account_usage":state.allowance,"server_errors":state.errors,"connection_error":state.ended,"reconnects_for_new_threads":self.0.reconnect,
            "workspace_mode":"prepared_directory", "max_threads":8,
            "note":"Paths are on the server. Prepare an existing subdirectory of its workspace root. Files survive thread deletion."}),
        )
    }

    pub(crate) fn list(&self) -> Value {
        let state = self.0.state.lock().unwrap();
        let mut value =
            json!({"threads":state.threads.values().map(Thread::brief).collect::<Vec<_>>()});
        if !state.completed.is_empty() {
            value["results"] = json!(state.completed.iter().map(|(id, result)| json!({"thread_id":id,"name":result["name"],"status":result["status"],"closed":result["closed"]})).collect::<Vec<_>>());
        }
        if let Some(error) = &state.ended {
            value["connection_error"] = json!(error);
        }
        if !state.errors.is_empty() {
            value["server_errors"] = json!(state.errors);
        }
        value
    }

    pub(crate) fn owns(&self, id: &str) -> Result<()> {
        let state = self.0.state.lock().unwrap();
        let thread = state
            .threads
            .get(id)
            .context("Unknown thread; it must belong to this MCP connection")?;
        ensure!(
            thread.status != "disconnected",
            "This thread's connection ended; create a new thread. Old prompts are never resent."
        );
        Ok(())
    }

    pub(crate) async fn read(
        &self,
        id: &str,
        cursor: Option<&str>,
        detail: Detail,
        wait_ms: u64,
        max_bytes: usize,
    ) -> Result<Value> {
        ensure!(wait_ms <= 30_000, "wait_ms must be at most 30000");
        ensure!(
            (256..=24_000).contains(&max_bytes),
            "max_bytes must be between 256 and 24000"
        );
        let mut changed = self.0.changed.subscribe();
        let deadline = Instant::now() + Duration::from_millis(wait_ms);
        loop {
            let (page, ready) = {
                let state = self.0.state.lock().unwrap();
                let thread = state
                    .threads
                    .get(id)
                    .context("Unknown thread; it must belong to this MCP connection")?;
                let mut page = thread.page(cursor, detail, max_bytes)?;
                page["server_errors"] = json!(state.errors);
                let ready = !page["events"].as_array().unwrap().is_empty()
                    || !thread.active()
                    || !thread.requests.is_empty();
                (page, ready)
            };
            if ready || Instant::now() >= deadline {
                return Ok(page);
            }
            if timeout_at(deadline, changed.changed()).await.is_err() {
                return Ok(page);
            }
        }
    }
}

impl Inner {
    fn disconnected(&self, ended: String) {
        {
            let mut state = self.state.lock().unwrap();
            state.commands = None;
            state.welcome = None;
            state.models = None;
            state.allowance = None;
            state.ended = Some(ended.clone());
            let ids: Vec<_> = state
                .threads
                .iter_mut()
                .filter_map(|(id, thread)| {
                    if thread.status == "disconnected" {
                        return None;
                    }
                    thread.disconnected(&ended);
                    Some(id.clone())
                })
                .collect();
            state.retired.extend(ids);
            while state.retired.len() > 8 {
                if let Some(id) = state.retired.pop_front() {
                    state.threads.remove(&id);
                }
            }
        }
        for (_, pending) in self.pending.lock().unwrap().drain() {
            let _ = pending.reply.send(Err(format!("{ended}. An in-flight command may have completed; it will not be replayed. The server cleans up this connection's threads.")));
        }
        self.changed.send_replace(());
    }

    fn event(&self, event: ServerEvent) {
        if let ServerEvent::CommandResult { command_id, event } = event {
            let result = match event.as_deref() {
                Some(
                    ServerEvent::Error { message, .. }
                    | ServerEvent::PromptRejected { message, .. },
                ) => Err(message.clone()),
                Some(event) => serde_json::to_value(event).map_err(|error| error.to_string()),
                None => Ok(json!({"acknowledged":true})),
            };
            if let Some(event) = event {
                self.ingest(*event);
            }
            if let Some(pending) = self.pending.lock().unwrap().remove(&command_id) {
                if let Some(thread_id) = pending.prompt_thread
                    && let Some(thread) = self.state.lock().unwrap().threads.get_mut(&thread_id)
                {
                    thread.submissions.remove(&command_id);
                }
                let _ = pending.reply.send(result);
            }
        } else {
            self.ingest(event);
        }
        self.changed.send_replace(());
    }

    fn ingest(&self, event: ServerEvent) {
        let mut state = self.state.lock().unwrap();
        match event {
            ServerEvent::Welcome {
                workspace,
                server_version,
            } => {
                state.welcome = Some(json!({"workspace_root":workspace,"version":server_version}));
                state.ended = None;
                state.errors.clear();
            }
            ServerEvent::Models {
                models,
                default_model,
            } => state.models = Some(json!({"models":models,"default_model":default_model})),
            ServerEvent::AccountUsage { usage } => state.allowance = Some(json!(usage)),
            event @ ServerEvent::SessionStarted { .. } => {
                let thread = Thread::started(event);
                state.threads.insert(thread.id.clone(), thread);
            }
            ServerEvent::SessionClosed { session_id } => {
                state.threads.remove(&session_id);
                if let Some((_, result)) =
                    state.completed.iter_mut().find(|(id, _)| id == &session_id)
                {
                    result["closed"] = json!(true);
                    result.as_object_mut().unwrap().remove("cleanup_error");
                }
            }
            ServerEvent::Error {
                session_id: None,
                message,
            } => {
                state.errors.push_back(message.clone());
                while state.errors.len() > 16 {
                    state.errors.pop_front();
                }
                for thread in state.threads.values_mut() {
                    thread.event(ServerEvent::Error {
                        session_id: Some(thread.id.clone()),
                        message: message.clone(),
                    });
                    thread.status = "backend_failed".into();
                }
            }
            event => {
                if let Some(thread) = event.session_id().and_then(|id| state.threads.get_mut(id)) {
                    thread.event(event);
                }
            }
        }
    }
}
