//! A small, version-tolerant client for Codex's documented app-server protocol.
//!
//! The journal contains IDs, never prompts. An ID leaves the journal only after
//! Codex confirms permanent deletion. No directory scan of Codex's own storage is
//! used: recovery can only delete threads that this bridge created and recorded.

use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Component, Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex as StdMutex, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::{Mutex, Notify, broadcast, oneshot},
    time::timeout,
};
use uuid::Uuid;

use crate::{
    account_usage::UsageCache,
    protocol::{AccountUsage, ModelInfo, ServerEvent, SessionOptions},
};

const RPC_TIMEOUT: Duration = Duration::from_secs(30);
const INTERRUPT_TIMEOUT: Duration = Duration::from_secs(10);
const CREATION_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CODEX_LINE: u64 = 16 * 1024 * 1024;
const MAX_BUFFERED_NATIVE_BYTES: usize = 64 * 1024 * 1024;
const MAX_BUFFERED_NATIVE_EVENTS: usize = 512;

pub struct BridgeConfig {
    pub executable: PathBuf,
    /// Parent directory for persistent, separate session workspaces.
    pub workspace: PathBuf,
    pub state_dir: PathBuf,
    pub model: Option<String>,
}

#[derive(Clone)]
pub struct CodexBridge {
    inner: Arc<Inner>,
}

struct PendingCall {
    method: String,
    sender: oneshot::Sender<std::result::Result<Value, CallFailure>>,
}

#[derive(Debug)]
enum CallFailure {
    Rejected(Value),
    Connection(String),
}

impl std::fmt::Display for CallFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected(error) => write!(formatter, "{error}"),
            Self::Connection(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for CallFailure {}

impl From<String> for CallFailure {
    fn from(message: String) -> Self {
        Self::Connection(message)
    }
}

impl From<&str> for CallFailure {
    fn from(message: &str) -> Self {
        Self::Connection(message.into())
    }
}

#[derive(Default)]
struct CreationWork {
    pending: HashSet<u64>,
    unclaimed: HashSet<String>,
    cleanup_tasks: usize,
}

#[derive(Default)]
struct BufferedNativeEvents {
    events: Vec<ServerEvent>,
    bytes: usize,
}

#[derive(Default)]
struct NativePublication {
    published: HashSet<String>,
    buffered: HashMap<String, BufferedNativeEvents>,
    buffered_bytes: usize,
    buffered_count: usize,
    overflowed: bool,
}

impl NativePublication {
    fn take_buffer(&mut self, id: &str) -> BufferedNativeEvents {
        let buffer = self.buffered.remove(id).unwrap_or_default();
        self.buffered_bytes -= buffer.bytes;
        self.buffered_count -= buffer.events.len();
        buffer
    }

    fn discard(&mut self, id: &str) {
        self.published.remove(id);
        self.take_buffer(id);
    }
}

struct Inner {
    stdin: Mutex<Option<ChildStdin>>,
    child: Mutex<Option<Child>>,
    pending: StdMutex<HashMap<u64, PendingCall>>,
    requests: StdMutex<HashMap<String, PendingRequest>>,
    sessions: StdMutex<HashMap<String, Arc<Session>>>,
    creations: StdMutex<CreationWork>,
    native: StdMutex<NativePublication>,
    creations_changed: Notify,
    events: broadcast::Sender<ServerEvent>,
    next_id: AtomicU64,
    alive: AtomicBool,
    closing: AtomicBool,
    lifecycle: Mutex<()>,
    workspace: PathBuf,
    model: Option<String>,
    models: Mutex<Option<Vec<ModelInfo>>>,
    usage: StdMutex<UsageCache>,
    usage_read: Mutex<()>,
    state_dir: PathBuf,
    journal_dir: PathBuf,
    _journal_lock: File,
}

struct Session {
    workspace: PathBuf,
    model: Option<String>,
    effort: Option<String>,
    operation: Mutex<()>,
    turn: StdMutex<TurnState>,
    turn_changed: Notify,
    closing: AtomicBool,
}

#[derive(Default)]
struct TurnState {
    active: Option<String>,
    starting: bool,
    completed: HashSet<String>,
}

#[derive(Clone)]
struct PendingRequest {
    raw_id: Value,
    thread_id: String,
}

#[derive(Serialize, Deserialize)]
struct JournalEntry {
    thread_id: String,
}

impl CodexBridge {
    pub async fn start(config: BridgeConfig) -> Result<Self> {
        fs::create_dir_all(&config.workspace).context("Create session workspace root")?;
        let workspace = config
            .workspace
            .canonicalize()
            .context("Open session workspace root")?;
        ensure!(workspace.is_dir(), "Workspace root must be a directory");
        fs::create_dir_all(&config.state_dir).context("Create bridge state directory")?;
        let state_dir = config
            .state_dir
            .canonicalize()
            .context("Resolve bridge state directory")?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(config.state_dir.join("bridge.lock"))
            .context("Open bridge state lock")?;
        lock.try_lock()
            .context("Another server is using this state directory")?;
        let journal_dir = config.state_dir.join("owned-threads");
        fs::create_dir_all(&journal_dir).context("Create cleanup journal")?;

        let mut command = Command::new(&config.executable);
        command
            .args(["app-server", "--listen", "stdio://"])
            .current_dir(&workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Do not create console windows for the bundled Windows desktop app.
        #[cfg(windows)]
        command.creation_flags(0x08000000);
        let mut child = command.spawn().with_context(|| {
            format!(
                "Start {} app-server; install Codex and run codex login on the server",
                config.executable.display()
            )
        })?;
        let stdin = child.stdin.take().context("Codex stdin unavailable")?;
        let stdout = child.stdout.take().context("Codex stdout unavailable")?;
        let stderr = child.stderr.take().context("Codex stderr unavailable")?;
        let (events, _) = broadcast::channel(4096);
        let inner = Arc::new(Inner {
            stdin: Mutex::new(Some(stdin)),
            child: Mutex::new(Some(child)),
            pending: StdMutex::new(HashMap::new()),
            requests: StdMutex::new(HashMap::new()),
            sessions: StdMutex::new(HashMap::new()),
            creations: StdMutex::new(CreationWork::default()),
            native: StdMutex::new(NativePublication::default()),
            creations_changed: Notify::new(),
            events,
            next_id: AtomicU64::new(1),
            alive: AtomicBool::new(true),
            closing: AtomicBool::new(false),
            lifecycle: Mutex::new(()),
            workspace,
            model: config.model,
            models: Mutex::new(None),
            usage: StdMutex::new(UsageCache::default()),
            usage_read: Mutex::new(()),
            state_dir,
            journal_dir,
            _journal_lock: lock,
        });
        let weak = Arc::downgrade(&inner);
        tokio::spawn(async move {
            read_stdout(weak, stdout).await;
        });
        // Drain stderr without forwarding arbitrary diagnostic lines or credentials
        // into a remote client's transcript. Bounded chunks avoid oversized lines.
        tokio::spawn(async move {
            let mut stderr = stderr;
            let mut buf = [0u8; 8192];
            while matches!(stderr.read(&mut buf).await, Ok(n) if n > 0) {}
        });
        let bridge = Self { inner };
        let startup = async {
            bridge.rpc("initialize", json!({
                "clientInfo": { "name": "codex_lan", "title": "Codex LAN", "version": env!("CARGO_PKG_VERSION") },
                "capabilities": { "experimentalApi": true }
            })).await?;
            bridge.send(json!({"method":"initialized","params":{}})).await?;
            bridge.recover_journal().await?;
            Ok::<_, anyhow::Error>(())
        }.await;
        if let Err(error) = startup {
            bridge.terminate().await;
            return Err(error.context("Initialize Codex bridge"));
        }
        Ok(bridge)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ServerEvent> {
        self.inner.events.subscribe()
    }

    /// Account metadata only; this never starts a model turn. Failures become a
    /// sanitized unavailable state rather than leaking native account errors.
    pub async fn account_usage(&self) -> AccountUsage {
        let _read = self.inner.usage_read.lock().await;
        let epoch = self.inner.usage.lock().unwrap().begin_read();
        let response = self
            .rpc(
                "account/rateLimits/read",
                json!({"excludeResetCreditDetails":true}),
            )
            .await
            .ok();
        self.inner
            .usage
            .lock()
            .unwrap()
            .finish_read(epoch, response.as_ref())
    }

    pub fn session_workspace(&self, id: &str) -> Result<PathBuf> {
        Ok(self.session(id)?.workspace.clone())
    }

    pub fn session_model_effort(&self, id: &str) -> Result<(Option<String>, Option<String>)> {
        let session = self.session(id)?;
        Ok((session.model.clone(), session.effort.clone()))
    }

    /// Release native events only once the remote client has received its
    /// SessionStarted event. The lock keeps buffered and newly arriving events
    /// in order, without an await or an ownership gap during publication.
    pub fn publish_session(&self, id: &str) -> Result<()> {
        self.session(id)?;
        let mut native = self.inner.native.lock().unwrap();
        if !native.published.insert(id.into()) {
            return Ok(());
        }
        let buffer = native.take_buffer(id);
        for event in buffer.events {
            let _ = self.inner.events.send(event);
        }
        if native.overflowed {
            let _ = self.inner.events.send(ServerEvent::Error {
                session_id: Some(id.into()),
                message: "Native startup event buffering exceeded its limit; some startup metadata could not be retained".into(),
            });
        }
        if native.buffered.is_empty() {
            native.overflowed = false;
        }
        Ok(())
    }

    pub fn default_model(&self) -> Option<String> {
        self.inner.model.clone()
    }

    /// Read the native picker catalog without starting a model turn.
    pub async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let mut cached = self.inner.models.lock().await;
        if let Some(models) = cached.as_ref() {
            return Ok(models.clone());
        }
        let mut models = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen_cursors = HashSet::new();
        loop {
            let response = self
                .rpc(
                    "model/list",
                    json!({"limit":100,"includeHidden":false,"cursor":cursor}),
                )
                .await?;
            for model in response["data"]
                .as_array()
                .context("Codex returned no model catalog")?
            {
                if model["hidden"].as_bool() == Some(true) {
                    continue;
                }
                let Some(name) = model["model"].as_str().filter(|name| !name.is_empty()) else {
                    continue;
                };
                if models.iter().any(|entry: &ModelInfo| entry.model == name) {
                    continue;
                }
                models.push(ModelInfo {
                    model: name.into(),
                    display_name: model["displayName"].as_str().unwrap_or(name).into(),
                    efforts: model["supportedReasoningEfforts"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|effort| effort["reasoningEffort"].as_str().map(str::to_owned))
                        .collect(),
                    default_effort: model["defaultReasoningEffort"].as_str().map(str::to_owned),
                    is_default: model["isDefault"].as_bool().unwrap_or(false),
                });
            }
            cursor = response["nextCursor"]
                .as_str()
                .filter(|cursor| !cursor.is_empty())
                .map(str::to_owned);
            let Some(next) = cursor.as_ref() else { break };
            ensure!(
                seen_cursors.len() < 100 && seen_cursors.insert(next.clone()),
                "Codex model catalog pagination did not finish"
            );
        }
        *cached = Some(models.clone());
        Ok(models)
    }

    pub async fn refresh_models(&self) -> Result<Vec<ModelInfo>> {
        *self.inner.models.lock().await = None;
        self.list_models().await
    }

    pub async fn new_session(&self, name: &str) -> Result<String> {
        self.new_session_with_options(name, &SessionOptions::default())
            .await
    }

    pub async fn new_session_with_options(
        &self,
        name: &str,
        options: &SessionOptions,
    ) -> Result<String> {
        let _lifecycle = self.inner.lifecycle.lock().await;
        ensure!(
            !self.inner.closing.load(Ordering::SeqCst),
            "Server is shutting down"
        );
        ensure!(
            self.inner.creations.lock().unwrap().pending.is_empty(),
            "A previous session creation is still awaiting Codex; wait or restart the server before creating another"
        );
        let name = name.trim();
        ensure!(
            !name.is_empty() && name.len() <= 160,
            "Session name must contain 1–160 bytes"
        );
        let requested_model = options
            .model
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let effort = options
            .effort
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let model = requested_model
            .map(str::to_owned)
            .or_else(|| self.inner.model.clone());
        if requested_model.is_some() || effort.is_some() {
            ensure!(
                model.is_some(),
                "Choose a model before overriding effort; the native configured default may differ from the catalog default"
            );
            let models = self
                .list_models()
                .await
                .context("Read model choices before creating a session")?;
            let selected = models
                .iter()
                .find(|entry| Some(&entry.model) == model.as_ref())
                .context("Selected model is not available in the native Codex catalog")?;
            if let Some(effort) = effort {
                ensure!(
                    selected.efforts.iter().any(|supported| supported == effort),
                    "Effort '{effort}' is not supported by {}",
                    selected.model
                );
            }
        }
        let override_path = options
            .workspace
            .as_deref()
            .map(str::trim)
            .filter(|path| !path.is_empty());
        let workspace = if let Some(path) = override_path {
            let mut protected = vec![self.inner.state_dir.clone()];
            protected.extend(
                self.inner
                    .sessions
                    .lock()
                    .unwrap()
                    .values()
                    .map(|session| session.workspace.clone()),
            );
            resolve_session_workspace(&self.inner.workspace, path, &protected)?
        } else {
            create_session_workspace(&self.inner.workspace, name)?
        };
        ensure!(
            !self.inner.sessions.lock().unwrap().values().any(|session| {
                workspace.starts_with(&session.workspace)
                    || session.workspace.starts_with(&workspace)
            }),
            "This folder overlaps another open session's workspace; choose a different folder or close that session first"
        );
        let mut params = json!({
            "cwd": workspace,
            "sandbox": "workspace-write",
            "approvalPolicy": "never",
            "ephemeral": false,
            "serviceName": "codex_lan",
            "developerInstructions": format!(
                "Use {} as this session's working directory. Keep working files and downloads inside it. When saving a file, report its absolute path to the user.",
                workspace.display()
            ),
            "config": {
                "sandbox_workspace_write.writable_roots": [workspace],
                "sandbox_workspace_write.network_access": true,
                "sandbox_workspace_write.exclude_tmpdir_env_var": true,
                "sandbox_workspace_write.exclude_slash_tmp": true
            }
        });
        if let Some(model) = &model {
            params["model"] = json!(model);
        }
        if let Some(effort) = effort {
            params["config"]["model_reasoning_effort"] = json!(effort);
        }
        let response = self.rpc("thread/start", params).await?;
        let id = response
            .pointer("/thread/id")
            .and_then(Value::as_str)
            .context("Codex thread/start returned no thread ID")?
            .to_owned();
        // The stdout reader records returned IDs before delivery. If this future
        // is cancelled after delivery but before polling, shutdown still owns
        // the unclaimed ID and can delete it.
        ensure!(
            self.inner.creations.lock().unwrap().unclaimed.contains(&id),
            "New Codex thread is not recorded for cleanup"
        );
        let model = response["model"].as_str().map(str::to_owned).or(model);
        let effort = response["reasoningEffort"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| effort.map(str::to_owned));
        self.inner.sessions.lock().unwrap().insert(
            id.clone(),
            Arc::new(Session {
                workspace: workspace.clone(),
                model: model.clone(),
                effort: effort.clone(),
                operation: Mutex::new(()),
                turn: StdMutex::new(TurnState::default()),
                turn_changed: Notify::new(),
                closing: AtomicBool::new(false),
            }),
        );
        self.inner.creations.lock().unwrap().unclaimed.remove(&id);
        self.inner.creations_changed.notify_one();
        let _ = self.inner.events.send(ServerEvent::SessionStarted {
            session_id: id.clone(),
            name: name.into(),
            workspace: workspace.to_string_lossy().into_owned(),
            model,
            effort,
        });
        // Naming is cosmetic. Do not add an await between claiming the thread
        // above and returning its ID: cancellation there could prevent the
        // transport from learning that it owns the newly registered session.
        let bridge = self.clone();
        let named_id = id.clone();
        let native_name = format!("LAN / {name}");
        tokio::spawn(async move {
            if let Err(error) = bridge
                .rpc(
                    "thread/name/set",
                    json!({"threadId": named_id, "name": native_name}),
                )
                .await
            {
                bridge.activity(
                    &named_id,
                    format!("Could not set native Codex thread name: {error:#}"),
                );
            }
        });
        Ok(id)
    }

    /// Submit input now: steer a running turn, or begin a new turn when idle.
    /// A failed or timed-out submission is never silently retried unless Codex
    /// explicitly rejects steering because the previous turn has ended.
    pub async fn prompt(&self, id: &str, text: &str) -> Result<bool> {
        ensure!(!text.trim().is_empty(), "Prompt cannot be empty");
        ensure!(text.len() <= 256 * 1024, "Prompt exceeds 256 KiB");
        let session = self.session(id)?;
        let _operation = session.operation.lock().await;
        self.ensure_open(&session)?;
        self.publish_session(id)?;
        let active = session.turn.lock().unwrap().active.clone();
        if let Some(turn) = active {
            match self
                .rpc(
                    "turn/steer",
                    json!({
                        "threadId":id,
                        "expectedTurnId":turn,
                        "input":[{"type":"text","text":text}]
                    }),
                )
                .await
            {
                Ok(_) => return Ok(true),
                Err(error) => {
                    let rejected = error.downcast_ref::<CallFailure>().and_then(|failure| {
                        if let CallFailure::Rejected(value) = failure {
                            Some(value)
                        } else {
                            None
                        }
                    });
                    let mut state = session.turn.lock().unwrap();
                    let finished = state.completed.contains(&turn) && state.active.is_none();
                    let no_active = rejected
                        .and_then(|value| value["message"].as_str())
                        .is_some_and(|message| {
                            let message = message.to_ascii_lowercase();
                            message.contains("no active turn")
                                || message.contains("no in-progress turn")
                        });
                    let old_or_idle = state.active.as_deref().is_none_or(|active| active == turn);
                    if rejected.is_some() && old_or_idle && (finished || no_active) {
                        state.active = None;
                        state.starting = false;
                        state.completed.insert(turn.clone());
                    } else {
                        return Err(error);
                    }
                }
            }
        }
        {
            let mut state = session.turn.lock().unwrap();
            ensure!(
                !state.starting && state.active.is_none(),
                "Codex is still accepting the previous submission; wait for its result before retrying"
            );
            state.starting = true;
        }
        let mut params = json!({
            "threadId": id,
            "input": [{"type":"text","text":text}],
            "cwd": session.workspace,
            "approvalPolicy": "never",
            "sandboxPolicy": {
                "type":"workspaceWrite", "writableRoots":[session.workspace],
                "networkAccess":true, "excludeTmpdirEnvVar":true, "excludeSlashTmp":true
            }
        });
        if let Some(model) = &session.model {
            params["model"] = json!(model);
        }
        if let Some(effort) = &session.effort {
            params["effort"] = json!(effort);
        }
        let result = self.rpc("turn/start", params).await;
        let mut state = session.turn.lock().unwrap();
        state.starting = false;
        match result {
            Ok(value) => {
                let turn = value
                    .pointer("/turn/id")
                    .and_then(Value::as_str)
                    .context("Codex returned no turn ID")?;
                if !state.completed.contains(turn) {
                    state.active = Some(turn.to_owned());
                }
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    pub async fn interrupt(&self, id: &str) -> Result<()> {
        let session = self.session(id)?;
        let _operation = session.operation.lock().await;
        self.ensure_open(&session)?;
        self.interrupt_session(id, &session).await
    }

    pub async fn reply(&self, id: &str, request_id: &str, result: Value) -> Result<()> {
        let session = self.session(id)?;
        self.ensure_open(&session)?;
        let request = {
            let mut requests = self.inner.requests.lock().unwrap();
            let request = requests
                .get(request_id)
                .context("This Codex question is no longer pending")?;
            ensure!(
                request.thread_id == id,
                "Question belongs to another session"
            );
            requests.remove(request_id).unwrap()
        };
        if let Err(error) = self
            .send(json!({"id":request.raw_id,"result":result}))
            .await
        {
            self.inner
                .requests
                .lock()
                .unwrap()
                .insert(request_id.into(), request);
            return Err(error);
        }
        let _ = self.inner.events.send(ServerEvent::RequestResolved {
            session_id: id.into(),
            request_id: request_id.into(),
        });
        Ok(())
    }

    pub async fn close_session(&self, id: &str) -> Result<()> {
        let session = self.session(id)?;
        let _operation = session.operation.lock().await;
        session.closing.store(true, Ordering::SeqCst);
        // Deletion is still attempted if a previous operation failed. The journal
        // must survive any uncertain result so next startup can retry safely.
        if let Err(error) = self.interrupt_session(id, &session).await {
            self.activity(
                id,
                format!("Stopping session: {error:#}; attempting deletion"),
            );
        }
        let clean = self
            .rpc("thread/backgroundTerminals/clean", json!({"threadId":id}))
            .await;
        if let Err(error) = clean
            && !is_unsupported(&error)
        {
            self.activity(id, format!("Terminal cleanup: {error:#}"));
        }
        self.rpc("thread/delete", json!({"threadId":id}))
            .await
            .context("Codex thread deletion failed; its ID remains in the cleanup journal")?;
        self.forget(id)?;
        self.inner.sessions.lock().unwrap().remove(id);
        self.inner.native.lock().unwrap().discard(id);
        self.inner
            .requests
            .lock()
            .unwrap()
            .retain(|_, request| request.thread_id != id);
        let _ = self.inner.events.send(ServerEvent::SessionClosed {
            session_id: id.into(),
        });
        Ok(())
    }

    pub async fn shutdown(&self) -> Result<()> {
        let _lifecycle = self.inner.lifecycle.lock().await;
        self.inner.closing.store(true, Ordering::SeqCst);
        let ids: Vec<_> = self
            .inner
            .sessions
            .lock()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        let mut errors = Vec::new();
        for id in ids {
            if let Err(error) = self.close_session(&id).await {
                errors.push(format!("{id}: {error:#}"));
            }
        }
        match timeout(CREATION_DRAIN_TIMEOUT, self.drain_creations()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => errors.push(format!("Unfinished session creation: {error:#}")),
            Err(_) => errors.push(format!(
                "Timed out draining session creation after {} seconds; a Codex thread may exist without a confirmed deletion", CREATION_DRAIN_TIMEOUT.as_secs()
            )),
        }
        self.terminate().await;
        let work = self.inner.creations.lock().unwrap();
        if !work.pending.is_empty() || !work.unclaimed.is_empty() || work.cleanup_tasks != 0 {
            errors.push(format!(
                "Unresolved creation cleanup: {} unknown thread/start results, {} known unclaimed threads, {} cleanup tasks",
                work.pending.len(), work.unclaimed.len(), work.cleanup_tasks
            ));
        }
        drop(work);
        match fs::read_dir(&self.inner.journal_dir) {
            Ok(records) => {
                let mut remaining = 0;
                for record in records {
                    match record {
                        Ok(entry)
                            if entry.path().extension().and_then(|ext| ext.to_str())
                                == Some("json") =>
                        {
                            remaining += 1
                        }
                        Ok(_) => {}
                        Err(error) => {
                            errors.push(format!("Could not inspect cleanup record: {error}"))
                        }
                    }
                }
                if remaining != 0 {
                    errors.push(format!(
                        "{remaining} owned-thread cleanup records remain for next startup"
                    ));
                }
            }
            Err(error) => errors.push(format!("Could not verify cleanup journal: {error}")),
        }
        if !errors.is_empty() {
            bail!(
                "Cleanup is incomplete; recorded IDs retry on next start, while unknown creation results may need manual inspection: {}",
                errors.join("; ")
            );
        }
        Ok(())
    }

    async fn drain_creations(&self) -> Result<()> {
        loop {
            let changed = self.inner.creations_changed.notified();
            let settled = {
                let work = self.inner.creations.lock().unwrap();
                work.pending.is_empty() && work.cleanup_tasks == 0
            };
            if settled {
                break;
            }
            ensure!(
                self.inner.alive.load(Ordering::SeqCst),
                "Codex exited with unresolved thread creation or cleanup"
            );
            changed.await;
        }
        let unclaimed: Vec<_> = self
            .inner
            .creations
            .lock()
            .unwrap()
            .unclaimed
            .iter()
            .cloned()
            .collect();
        for id in unclaimed {
            self.delete_unclaimed(&id).await?;
        }
        Ok(())
    }

    async fn delete_unclaimed(&self, id: &str) -> Result<()> {
        self.rpc("thread/delete", json!({"threadId":id}))
            .await
            .with_context(|| format!("Unclaimed Codex thread {id} could not be deleted"))?;
        self.forget(id)?;
        self.inner.creations.lock().unwrap().unclaimed.remove(id);
        self.inner.native.lock().unwrap().discard(id);
        self.inner.creations_changed.notify_one();
        Ok(())
    }

    fn session(&self, id: &str) -> Result<Arc<Session>> {
        self.inner
            .sessions
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .context("Unknown or closed session")
    }

    fn ensure_open(&self, session: &Session) -> Result<()> {
        ensure!(
            !self.inner.closing.load(Ordering::SeqCst),
            "Server is shutting down"
        );
        ensure!(
            !session.closing.load(Ordering::SeqCst),
            "Session is closing"
        );
        Ok(())
    }

    fn activity(&self, id: &str, text: String) {
        let _ = self.inner.events.send(ServerEvent::Activity {
            session_id: id.into(),
            text,
        });
    }

    async fn interrupt_session(&self, id: &str, session: &Session) -> Result<()> {
        let active = session.turn.lock().unwrap().active.clone();
        if let Some(turn) = active {
            self.rpc("turn/interrupt", json!({"threadId":id,"turnId":turn}))
                .await?;
            timeout(INTERRUPT_TIMEOUT, async {
                loop {
                    let notified = session.turn_changed.notified();
                    if session.turn.lock().unwrap().active.is_none() {
                        break;
                    }
                    notified.await;
                }
            })
            .await
            .context("Codex did not confirm interruption in time")?;
        }
        Ok(())
    }

    async fn send(&self, value: Value) -> Result<()> {
        ensure!(
            self.inner.alive.load(Ordering::SeqCst),
            "Codex process is not running"
        );
        let mut bytes = serde_json::to_vec(&value)?;
        bytes.push(b'\n');
        let mut stdin = self.inner.stdin.lock().await;
        let stdin = stdin.as_mut().context("Codex stdin is closed")?;
        timeout(RPC_TIMEOUT, async {
            stdin.write_all(&bytes).await?;
            stdin.flush().await
        })
        .await
        .context("Timed out writing to Codex")?
        .context("Write to Codex")
    }

    async fn rpc(&self, method: &str, params: Value) -> Result<Value> {
        ensure!(
            self.inner.alive.load(Ordering::SeqCst),
            "Codex process is not running"
        );
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, mut receiver) = oneshot::channel();
        if method == "thread/start" {
            self.inner.creations.lock().unwrap().pending.insert(id);
        }
        self.inner.pending.lock().unwrap().insert(
            id,
            PendingCall {
                method: method.into(),
                sender,
            },
        );
        if let Err(error) = self
            .send(json!({"id":id,"method":method,"params":params}))
            .await
        {
            if method != "thread/start" {
                self.inner.pending.lock().unwrap().remove(&id);
            }
            return Err(error);
        }
        let response = timeout(RPC_TIMEOUT, &mut receiver).await;
        match response {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(error))) => Err(anyhow::Error::new(error).context(method.to_owned())),
            Ok(Err(_)) => Err(anyhow!("{method}: Codex connection closed")),
            Err(_) => {
                // Closing first makes a response concurrent with the timeout
                // either observable here or undeliverable in read_stdout. A
                // late thread/start response can therefore still be cleaned.
                receiver.close();
                if let Ok(result) = receiver.try_recv() {
                    return result
                        .map_err(|error| anyhow::Error::new(error).context(method.to_owned()));
                }
                if method != "thread/start" {
                    self.inner.pending.lock().unwrap().remove(&id);
                }
                Err(anyhow!(
                    "{method}: Codex did not respond within {} seconds",
                    RPC_TIMEOUT.as_secs()
                ))
            }
        }
    }

    async fn terminate(&self) {
        self.inner.closing.store(true, Ordering::SeqCst);
        self.inner.stdin.lock().await.take();
        if let Some(mut child) = self.inner.child.lock().await.take()
            && timeout(Duration::from_secs(3), child.wait()).await.is_err()
        {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        fail_connection(&self.inner, "Codex process stopped");
    }

    fn record(&self, id: &str) -> Result<()> {
        let path = journal_path(&self.inner.journal_dir, id);
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(path)
            .context("Create cleanup record")?;
        file.write_all(&serde_json::to_vec(&JournalEntry {
            thread_id: id.into(),
        })?)?;
        file.sync_all().context("Sync cleanup record")?;
        sync_directory(&self.inner.journal_dir)?;
        Ok(())
    }

    fn forget(&self, id: &str) -> Result<()> {
        match fs::remove_file(journal_path(&self.inner.journal_dir, id)) {
            Ok(()) => sync_directory(&self.inner.journal_dir),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => {
                Err(error).context("Thread deleted, but cleanup journal could not be updated")
            }
        }
    }

    async fn recover_journal(&self) -> Result<()> {
        let mut errors = Vec::new();
        for entry in fs::read_dir(&self.inner.journal_dir)? {
            let path = entry?.path();
            if path.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            let record: JournalEntry =
                serde_json::from_slice(&fs::read(&path)?).with_context(|| {
                    format!(
                        "Invalid cleanup record {}; inspect it before retrying",
                        path.display()
                    )
                })?;
            ensure!(
                path == journal_path(&self.inner.journal_dir, &record.thread_id),
                "Cleanup record name does not match its thread ID"
            );
            match self
                .rpc("thread/delete", json!({"threadId":record.thread_id}))
                .await
            {
                Ok(_) => self.forget(&record.thread_id)?,
                Err(error) => errors.push(format!("{}: {error:#}", record.thread_id)),
            }
        }
        ensure!(
            errors.is_empty(),
            "Cannot recover owned Codex threads: {}",
            errors.join("; ")
        );
        Ok(())
    }
}

fn resolve_session_workspace(
    root: &Path,
    requested: &str,
    protected: &[PathBuf],
) -> Result<PathBuf> {
    ensure!(
        requested.len() <= 4096 && !requested.contains('\0'),
        "Invalid session directory"
    );
    // A path is interpreted on the server. Reject Windows syntax on Unix rather
    // than silently creating a misleading directory named C:\\something.
    #[cfg(not(windows))]
    ensure!(
        !requested.contains(['\\', ':']),
        "Use a server path inside the workspace root"
    );
    let requested = Path::new(requested);
    ensure!(
        !requested
            .components()
            .any(|component| matches!(component, Component::ParentDir)),
        "Session directory cannot contain '..'"
    );
    ensure!(
        requested.is_absolute()
            || !requested
                .components()
                .any(|component| matches!(component, Component::Prefix(_) | Component::RootDir)),
        "Session directory must be relative to the root or an absolute server path inside it"
    );
    let candidate = if requested.is_absolute() {
        requested.to_owned()
    } else {
        root.join(requested)
    };
    // Resolve the nearest existing ancestor before creating anything. This
    // catches symlinks/junctions that leave the root, including when the final
    // child does not exist yet. Canonicalization also handles Windows' extended
    // path prefix when the user entered an ordinary C:\\... absolute path.
    let mut ancestor = candidate.as_path();
    let mut missing = Vec::new();
    loop {
        match fs::symlink_metadata(ancestor) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(
                    ancestor
                        .file_name()
                        .context("Invalid session directory")?
                        .to_owned(),
                );
                ancestor = ancestor
                    .parent()
                    .context("Session directory has no existing parent")?;
            }
            Err(error) => return Err(error).context("Inspect session directory"),
        }
    }
    let mut directory = ancestor
        .canonicalize()
        .context("Resolve session directory ancestor")?;
    ensure!(
        directory.is_dir() && directory.starts_with(root),
        "Session directory must stay inside the server workspace root"
    );
    let mut destination = directory.clone();
    for component in missing.iter().rev() {
        destination.push(component);
    }
    ensure!(
        destination != root,
        "Choose a subfolder; the workspace root itself cannot be used as a session directory"
    );
    ensure!(
        !protected
            .iter()
            .any(|path| destination.starts_with(path) || path.starts_with(&destination)),
        "Session directory overlaps server state or another open session's workspace"
    );
    for component in missing.into_iter().rev() {
        directory = directory
            .canonicalize()
            .context("Resolve session directory parent")?;
        ensure!(
            directory.starts_with(root),
            "Session directory left the server workspace root"
        );
        directory.push(component);
        match fs::create_dir(&directory) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error).context("Create session directory"),
        }
        directory = directory
            .canonicalize()
            .context("Resolve created session directory")?;
        ensure!(
            directory.is_dir() && directory.starts_with(root),
            "Session directory must stay inside the server workspace root"
        );
    }
    ensure!(
        directory != root,
        "Choose a subfolder; the workspace root itself cannot be used as a session directory"
    );
    ensure!(
        !protected
            .iter()
            .any(|path| directory.starts_with(path) || path.starts_with(&directory)),
        "Session directory overlaps server state or another open session's workspace"
    );
    Ok(directory)
}

fn create_session_workspace(root: &Path, name: &str) -> Result<PathBuf> {
    let mut slug = String::new();
    for character in name.chars() {
        if slug.len() >= 40 {
            break;
        }
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_end_matches('-');
    let slug = if slug.is_empty() { "thread" } else { slug };
    // The fixed prefix also avoids Windows device names such as CON and NUL.
    // Exclusive creation ensures an existing directory is never reused.
    let workspace = root.join(format!("session-{slug}-{}", Uuid::new_v4()));
    fs::create_dir(&workspace)
        .with_context(|| format!("Create session workspace {}", workspace.display()))?;
    let workspace = workspace
        .canonicalize()
        .context("Resolve new session workspace")?;
    ensure!(
        workspace.parent() == Some(root),
        "Session workspace must be directly inside the configured root"
    );
    // Session directories are user artifacts. They survive failed creation,
    // native thread deletion, and shutdown; only cleanup journal files are erased.
    Ok(workspace)
}

fn journal_path(directory: &Path, id: &str) -> PathBuf {
    directory.join(format!("{:x}.json", Sha256::digest(id.as_bytes())))
}

fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn is_unsupported(error: &anyhow::Error) -> bool {
    let text = format!("{error:#}").to_ascii_lowercase();
    text.contains("-32601")
        || text.contains("unknown method")
        || text.contains("method not found")
        || text.contains("unknown variant")
}

fn fail_connection(inner: &Inner, message: &str) {
    if !inner.alive.swap(false, Ordering::SeqCst) {
        return;
    }
    for (_, call) in inner.pending.lock().unwrap().drain() {
        let _ = call.sender.send(Err(message.into()));
    }
    inner.creations_changed.notify_one();
    if !inner.closing.load(Ordering::SeqCst) {
        let _ = inner.events.send(ServerEvent::Error {
            session_id: None,
            message: format!("{message}. End this server session and restart to retry cleanup."),
        });
    }
}

async fn read_stdout(weak: Weak<Inner>, stdout: tokio::process::ChildStdout) {
    let mut reader = BufReader::new(stdout);
    loop {
        let mut line = Vec::new();
        let read = (&mut reader)
            .take(MAX_CODEX_LINE + 1)
            .read_until(b'\n', &mut line)
            .await;
        let Some(inner) = weak.upgrade() else {
            break;
        };
        match read {
            Ok(0) => {
                fail_connection(&inner, "Codex process exited");
                break;
            }
            Ok(size) if size as u64 > MAX_CODEX_LINE => {
                fail_connection(&inner, "Codex sent an oversized protocol message");
                break;
            }
            Err(error) => {
                fail_connection(&inner, &format!("Codex output failed: {error}"));
                break;
            }
            _ => {}
        }
        let value: Value = match serde_json::from_slice(&line) {
            Ok(value) => value,
            Err(error) => {
                fail_connection(&inner, &format!("Invalid Codex protocol message: {error}"));
                break;
            }
        };
        if let Some(method) = value.get("method").and_then(Value::as_str) {
            if value.get("id").is_some() {
                if let Some(params) = value.get("params") {
                    emit_native_event(&inner, method, params);
                }
                let method = method.to_owned();
                // Even a stalled stdin writer must not stop us draining stdout;
                // Codex can send requests before responding to our own RPCs.
                tokio::spawn(async move {
                    handle_request(inner, &method, &value).await;
                });
            } else {
                handle_notification(&inner, method, value.get("params").unwrap_or(&Value::Null));
            }
        } else if let Some(id) = value.get("id").and_then(Value::as_u64) {
            let call = inner.pending.lock().unwrap().remove(&id);
            if let Some(call) = call {
                let response = if let Some(error) = value.get("error") {
                    Err(CallFailure::Rejected(error.clone()))
                } else {
                    Ok(value.get("result").cloned().unwrap_or(Value::Null))
                };
                if call.method == "thread/start" {
                    handle_creation_response(inner, id, call.sender, response);
                } else {
                    let _ = call.sender.send(response);
                }
            }
        }
    }
}

fn handle_creation_response(
    inner: Arc<Inner>,
    request_id: u64,
    sender: oneshot::Sender<std::result::Result<Value, CallFailure>>,
    response: std::result::Result<Value, CallFailure>,
) {
    let response = match response {
        Ok(value) => value,
        Err(error) => {
            inner.creations.lock().unwrap().pending.remove(&request_id);
            prune_native_buffers(&inner);
            inner.creations_changed.notify_one();
            let _ = sender.send(Err(error));
            return;
        }
    };
    let Some(id) = response
        .pointer("/thread/id")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        // A successful create without an ID is indeterminate. Preserve the
        // outstanding marker so shutdown cannot claim successful cleanup.
        let _ = sender.send(Err(
            "Codex thread/start returned no thread ID; cleanup cannot be confirmed".into(),
        ));
        inner.creations_changed.notify_one();
        return;
    };
    let bridge = CodexBridge {
        inner: inner.clone(),
    };
    inner.creations.lock().unwrap().unclaimed.insert(id.clone());
    let needs_cleanup = match bridge.record(&id) {
        Ok(()) => sender.send(Ok(response)).is_err(),
        Err(error) => {
            let _ = sender.send(Err(format!(
                "Cannot record new Codex thread {id}: {error:#}; attempting deletion"
            )
            .into()));
            true
        }
    };
    {
        let mut work = inner.creations.lock().unwrap();
        if needs_cleanup {
            work.cleanup_tasks += 1;
        }
        work.pending.remove(&request_id);
    }
    inner.creations_changed.notify_one();
    prune_native_buffers(&inner);
    if needs_cleanup {
        tokio::spawn(async move {
            if let Err(error) = bridge.delete_unclaimed(&id).await {
                let _ = bridge.inner.events.send(ServerEvent::Error {
                    session_id:None, message:format!("Cancelled session cleanup failed: {error:#}. Its ID remains tracked for shutdown and journal recovery.")
                });
            }
            bridge.inner.creations.lock().unwrap().cleanup_tasks -= 1;
            bridge.inner.creations_changed.notify_one();
        });
    }
}

async fn handle_request(inner: Arc<Inner>, method: &str, value: &Value) {
    let raw_id = value["id"].clone();
    let request_id = raw_id.to_string();
    let params = value.get("params").cloned().unwrap_or(Value::Null);
    let thread_id = native_thread_id(&params).unwrap_or("").to_owned();
    let is_owned = inner
        .sessions
        .lock()
        .unwrap()
        .get(&thread_id)
        .is_some_and(|session| !session.closing.load(Ordering::SeqCst));
    let bridge = CodexBridge {
        inner: inner.clone(),
    };
    // A client cannot broaden the server's workspace policy via an approval.
    let automatic = match method {
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
            Some(json!({"decision":"decline"}))
        }
        "item/permissions/requestApproval" => Some(json!({"permissions":{},"scope":"turn"})),
        _ => None,
    };
    if let Some(result) = automatic {
        let _ = bridge.send(json!({"id":raw_id,"result":result})).await;
        if is_owned {
            bridge.activity(&thread_id, "Codex requested permissions outside the configured workspace policy; request declined".into());
        }
    } else if is_owned
        && matches!(
            method,
            "item/tool/requestUserInput" | "mcpServer/elicitation/request"
        )
    {
        inner.requests.lock().unwrap().insert(
            request_id.clone(),
            PendingRequest {
                raw_id,
                thread_id: thread_id.clone(),
            },
        );
        let _ = inner.events.send(ServerEvent::Request {
            session_id: thread_id,
            request_id,
            method: method.into(),
            params,
        });
    } else {
        let _ = bridge.send(json!({"id":raw_id,"error":{"code":-32601,"message":"This client does not implement this server request"}})).await;
        if is_owned {
            bridge.activity(
                &thread_id,
                format!("Unsupported interactive request: {method}"),
            );
        }
    }
}

fn native_thread_id(params: &Value) -> Option<&str> {
    params
        .get("threadId")
        .and_then(Value::as_str)
        .or_else(|| params.pointer("/thread/id").and_then(Value::as_str))
        .or_else(|| params.get("thread_id").and_then(Value::as_str))
}

fn prune_native_buffers(inner: &Inner) {
    let mut retained = {
        let creations = inner.creations.lock().unwrap();
        if !creations.pending.is_empty() {
            return;
        }
        creations.unclaimed.clone()
    };
    retained.extend(inner.sessions.lock().unwrap().keys().cloned());
    let mut native = inner.native.lock().unwrap();
    let discarded: Vec<_> = native
        .buffered
        .keys()
        .filter(|id| !retained.contains(*id))
        .cloned()
        .collect();
    for id in discarded {
        native.take_buffer(&id);
    }
}

fn emit_native_event(inner: &Inner, method: &str, params: &Value) {
    let Some(id) = native_thread_id(params) else {
        return;
    };
    let known = inner.sessions.lock().unwrap().contains_key(id);
    if !known {
        let creations = inner.creations.lock().unwrap();
        if !creations.unclaimed.contains(id) && creations.pending.is_empty() {
            return;
        }
    }
    // Creation notifications can precede the thread/start response. Hold them
    // privately by native ID until that exact ID is claimed and published.
    // Unknown IDs are never released and are pruned when creation settles.
    let mut native = inner.native.lock().unwrap();
    let event = ServerEvent::NativeEvent {
        session_id: id.into(),
        method: method.into(),
        params: params.clone(),
    };
    if native.published.contains(id) {
        let _ = inner.events.send(event);
        return;
    }
    let bytes = serde_json::to_vec(&event)
        .map(|bytes| bytes.len())
        .unwrap_or(usize::MAX);
    if native.buffered_count >= MAX_BUFFERED_NATIVE_EVENTS
        || bytes > MAX_BUFFERED_NATIVE_BYTES.saturating_sub(native.buffered_bytes)
    {
        native.overflowed = true;
        return;
    }
    native.buffered_bytes += bytes;
    native.buffered_count += 1;
    let buffer = native.buffered.entry(id.into()).or_default();
    buffer.bytes += bytes;
    buffer.events.push(event);
}

fn handle_notification(inner: &Arc<Inner>, method: &str, params: &Value) {
    if method == "account/rateLimits/updated" {
        if let Some(usage) = inner.usage.lock().unwrap().update(params) {
            let _ = inner.events.send(ServerEvent::AccountUsage { usage });
        }
        return;
    }
    if method == "account/updated" {
        let usage = inner.usage.lock().unwrap().account_changed();
        let _ = inner.events.send(ServerEvent::AccountUsage { usage });
        let bridge = CodexBridge {
            inner: inner.clone(),
        };
        tokio::spawn(async move {
            let usage = bridge.account_usage().await;
            let _ = bridge
                .inner
                .events
                .send(ServerEvent::AccountUsage { usage });
        });
        return;
    }
    emit_native_event(inner, method, params);
    let Some(id) = native_thread_id(params) else {
        return;
    };
    let Some(session) = inner.sessions.lock().unwrap().get(id).cloned() else {
        return;
    };
    let event = match method {
        "item/agentMessage/delta" => Some(ServerEvent::Delta {
            session_id: id.into(),
            item_id: params["itemId"].as_str().unwrap_or("message").into(),
            text: params["delta"].as_str().unwrap_or("").into(),
        }),
        "turn/started" => {
            if let Some(turn) = params.pointer("/turn/id").and_then(Value::as_str) {
                let mut state = session.turn.lock().unwrap();
                if state.completed.contains(turn) {
                    return;
                }
                state.active = Some(turn.into());
                state.starting = false;
            }
            Some(ServerEvent::TurnStarted {
                session_id: id.into(),
            })
        }
        "turn/completed" => {
            {
                let mut state = session.turn.lock().unwrap();
                let turn = params.pointer("/turn/id").and_then(Value::as_str);
                if let Some(turn) = turn {
                    state.completed.insert(turn.to_owned());
                    // Late completion from an older turn cannot clear a newer
                    // active turn or its pending user-input requests.
                    if state.active.as_deref().is_some_and(|active| active != turn) {
                        return;
                    }
                }
                state.active = None;
                state.starting = false;
            }
            session.turn_changed.notify_waiters();
            clear_requests(inner, id);
            if let Some(error) = params.pointer("/turn/error").filter(|e| !e.is_null()) {
                let _ = inner.events.send(ServerEvent::Error {
                    session_id: Some(id.into()),
                    message: error
                        .get("message")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .unwrap_or_else(|| error.to_string()),
                });
            }
            Some(ServerEvent::TurnFinished {
                session_id: id.into(),
                status: params
                    .pointer("/turn/status")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .into(),
            })
        }
        "item/completed"
            if params.pointer("/item/type").and_then(Value::as_str) == Some("agentMessage") =>
        {
            Some(ServerEvent::ItemCompleted {
                session_id: id.into(),
                item_id: params
                    .pointer("/item/id")
                    .and_then(Value::as_str)
                    .unwrap_or("message")
                    .into(),
                text: params
                    .pointer("/item/text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .into(),
            })
        }
        "item/started" | "item/completed" => {
            let item = &params["item"];
            let kind = item["type"].as_str().unwrap_or("activity");
            if matches!(kind, "userMessage" | "agentMessage" | "reasoning") {
                None
            } else {
                let action = if method == "item/started" {
                    "Started"
                } else {
                    "Finished"
                };
                let detail = item["command"].as_str().unwrap_or("");
                Some(ServerEvent::Activity {
                    session_id: id.into(),
                    text: format!("{action} {kind} {detail}").trim_end().to_owned(),
                })
            }
        }
        "serverRequest/resolved" => {
            let request_id = params["requestId"].to_string();
            inner.requests.lock().unwrap().remove(&request_id);
            Some(ServerEvent::RequestResolved {
                session_id: id.into(),
                request_id,
            })
        }
        "error" => Some(ServerEvent::Error {
            session_id: Some(id.into()),
            message: params
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("Codex reported an error")
                .into(),
        }),
        _ => None,
    };
    if let Some(event) = event {
        let _ = inner.events.send(event);
    }
}

fn clear_requests(inner: &Inner, id: &str) {
    let removed: Vec<_> = inner
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, request)| request.thread_id == id)
        .map(|(key, _)| key.clone())
        .collect();
    for request_id in removed {
        inner.requests.lock().unwrap().remove(&request_id);
        let _ = inner.events.send(ServerEvent::RequestResolved {
            session_id: id.into(),
            request_id,
        });
    }
}
