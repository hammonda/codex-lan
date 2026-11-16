//! Client session state and network lifecycle.

mod observer;
mod transcript;
mod view;
pub(crate) use observer::SessionObserver;

use crate::*;
use codex_lan_core::{
    protocol::{AccountUsage, ClientCommand, ModelInfo, ServerEvent, SessionOptions},
    transport::{self, ClientConfig},
};
use eframe::egui;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use tokio::sync::{mpsc, oneshot};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
enum OutputLevel {
    #[default]
    Replies,
    Progress,
    Tools,
    Everything,
}

impl OutputLevel {
    const ALL: [Self; 4] = [Self::Replies, Self::Progress, Self::Tools, Self::Everything];

    fn label(self) -> &'static str {
        match self {
            Self::Replies => "Replies",
            Self::Progress => "Progress",
            Self::Tools => "Tools",
            Self::Everything => "Everything",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Replies => {
                "Your prompts and Codex's text replies, including progress text. Errors and questions always remain visible."
            }
            Self::Progress => {
                "Replies, plans, emitted reasoning summaries, and compact tool/change headings. Use JSON to inspect an item's raw events."
            }
            Self::Tools => {
                "Readable tool output, file changes, and event details, with expandable bodies."
            }
            Self::Everything => {
                "Every retained native event as inspectable JSON, plus local input and errors. No extra model calls."
            }
        }
    }

    fn presentation(self, message: &Message) -> MessagePresentation {
        match (self, message.role) {
            (_, "ERROR" | "USER") => MessagePresentation::Full,
            (Self::Everything, _) => MessagePresentation::Hidden,
            (_, "AGENT") => MessagePresentation::Full,
            (Self::Replies, _) => MessagePresentation::Hidden,
            (Self::Progress, "TOOL" | "DIFF") => MessagePresentation::Summary,
            (Self::Progress, "EVENT") => MessagePresentation::Hidden,
            _ => MessagePresentation::Full,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MessagePresentation {
    Hidden,
    Summary,
    Full,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum InspectionScope {
    Item(String),
    TurnMethod {
        turn_id: String,
        method: &'static str,
    },
    Event(usize),
}

impl InspectionScope {
    fn matches(&self, index: usize, record: &NativeRecord) -> bool {
        match self {
            Self::Item(id) => {
                record.params["itemId"].as_str() == Some(id)
                    || record.params.pointer("/item/id").and_then(Value::as_str) == Some(id)
            }
            Self::TurnMethod { turn_id, method } => {
                record.method == *method && record.params["turnId"].as_str() == Some(turn_id)
            }
            Self::Event(event) => *event == index,
        }
    }
}

struct Message {
    role: &'static str,
    item_id: Option<String>,
    title: String,
    text: String,
    expanded: bool,
    progress_expanded: bool,
    full_text: bool,
    inspection: Option<InspectionScope>,
    inspecting: bool,
    // None means dirty; Some(None) remembers a non-JSON body too.
    json: Option<Option<Value>>,
}

impl Message {
    fn text(role: &'static str, text: impl Into<String>) -> Self {
        Self {
            role,
            item_id: None,
            title: String::new(),
            text: text.into(),
            expanded: true,
            progress_expanded: false,
            full_text: false,
            inspection: None,
            inspecting: false,
            json: None,
        }
    }

    fn replace_text(&mut self, text: String) {
        self.text = text;
        self.json = None;
    }

    fn append_text(&mut self, text: &str) {
        self.text.push_str(text);
        self.json = None;
    }

    fn prepare_json(&mut self) {
        if self.json.is_none() {
            let text = self.text.trim();
            let text = text
                .strip_prefix("```json\n")
                .or_else(|| text.strip_prefix("```JSON\n"))
                .or_else(|| text.strip_prefix("```\n"))
                .and_then(|text| text.strip_suffix("```"))
                .map(str::trim)
                .unwrap_or(text);
            self.json = Some(if text.starts_with(['{', '[']) {
                serde_json::from_str(text).ok()
            } else {
                None
            });
        }
    }

    fn copy_text(&self, presentation: MessagePresentation) -> String {
        match presentation {
            MessagePresentation::Hidden => String::new(),
            MessagePresentation::Summary if !self.progress_expanded => self.summary_title().into(),
            MessagePresentation::Summary | MessagePresentation::Full => {
                if self.title.is_empty() {
                    self.text.clone()
                } else {
                    format!("{}\n{}", self.title, self.text)
                }
            }
        }
    }

    fn summary_title(&self) -> &str {
        if !self.title.is_empty() {
            &self.title
        } else if self.role == "DIFF" {
            "file changes"
        } else {
            "tool activity"
        }
    }
}

struct Request {
    id: String,
    method: String,
    params: Value,
    answers: BTreeMap<String, String>,
    raw: String,
    error: Option<String>,
}

struct NativeRecord {
    method: String,
    params: Value,
}

struct Session {
    id: String,
    name: String,
    workspace: String,
    model: Option<String>,
    effort: Option<String>,
    messages: Vec<Message>,
    native: Vec<NativeRecord>,
    output_level: OutputLevel,
    usage: crate::usage::ThreadUsage,
    draft: String,
    active: bool,
    pending_prompts: usize,
    closing: bool,
    activity: String,
    requests: Vec<Request>,
}

impl Session {
    fn new(id: String, name: String, workspace: String) -> Self {
        Self {
            id,
            name,
            workspace,
            model: None,
            effort: None,
            messages: vec![],
            native: vec![],
            output_level: OutputLevel::default(),
            usage: crate::usage::ThreadUsage::default(),
            draft: String::new(),
            active: false,
            pending_prompts: 0,
            closing: false,
            activity: "ready".into(),
            requests: vec![],
        }
    }

    fn agent_text(&mut self, item_id: String, text: String, complete: bool) {
        if let Some(message) = self
            .messages
            .iter_mut()
            .find(|m| m.item_id.as_ref() == Some(&item_id))
        {
            if complete {
                message.replace_text(text);
            } else {
                message.append_text(&text);
            }
        } else {
            let mut message = Message::text("AGENT", text);
            message.inspection = Some(InspectionScope::Item(item_id.clone()));
            message.item_id = Some(item_id);
            self.messages.push(message);
        }
    }

    fn visible_output(&self) -> String {
        if self.output_level == OutputLevel::Everything {
            let local = self
                .messages
                .iter()
                .filter(|message| matches!(message.role, "USER" | "ERROR"))
                .map(|message| {
                    json!({"source":"client","role":message.role,"text":message.text}).to_string()
                });
            return local
                .chain(self.native.iter().map(|record| {
                    json!({"method":record.method,"params":record.params}).to_string()
                }))
                .collect::<Vec<_>>()
                .join("\n");
        }
        self.messages
            .iter()
            .filter_map(|message| {
                let presentation = self.output_level.presentation(message);
                (presentation != MessagePresentation::Hidden)
                    .then(|| format!("{}\n{}", message.role, message.copy_text(presentation)))
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    fn set_output_level(&mut self, level: OutputLevel) {
        if self.output_level != level {
            self.output_level = level;
            // A lower detail level should never inherit an open raw inspector.
            // The retained events and explicit tool expansion states stay intact.
            for message in &mut self.messages {
                message.inspecting = false;
            }
        }
    }

    fn native_message(&mut self, key: String, role: &'static str, title: String) -> &mut Message {
        let index = self
            .messages
            .iter()
            .position(|m| m.item_id.as_ref() == Some(&key))
            .unwrap_or_else(|| {
                let mut message = Message::text(role, "");
                message.inspection = key
                    .strip_prefix("native:")
                    .map(|id| InspectionScope::Item(id.into()));
                message.item_id = Some(key);
                message.title = title.clone();
                self.messages.push(message);
                self.messages.len() - 1
            });
        let message = &mut self.messages[index];
        message.role = role;
        if !title.is_empty() {
            message.title = title;
        }
        message
    }

    fn native_event(&mut self, method: String, params: Value) {
        let item_id = params["itemId"].as_str().unwrap_or("item");
        match method.as_str() {
            "item/started" | "item/completed" => {
                let item = &params["item"];
                let kind = item["type"].as_str().unwrap_or("item");
                if !matches!(kind, "userMessage" | "agentMessage") {
                    let key = format!("native:{}", item["id"].as_str().unwrap_or(item_id));
                    let (role, title, text) = describe_item(item);
                    let message = self.native_message(key, role, title);
                    // Some completion events omit/truncate aggregated output. Preserve streamed bytes.
                    message.replace_text(merge_snapshot(&message.text, &text));
                }
            }
            "item/commandExecution/outputDelta" | "item/fileChange/outputDelta" => {
                let message =
                    self.native_message(format!("native:{item_id}"), "TOOL", String::new());
                message.append_text(params["delta"].as_str().unwrap_or(""));
            }
            "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" => {
                let message =
                    self.native_message(format!("native:{item_id}"), "THOUGHT", "reasoning".into());
                message.append_text(params["delta"].as_str().unwrap_or(""));
            }
            "item/reasoning/summaryPartAdded" => {
                let message =
                    self.native_message(format!("native:{item_id}"), "THOUGHT", "reasoning".into());
                if !message.text.is_empty() && !message.text.ends_with('\n') {
                    message.append_text("\n");
                }
            }
            "turn/plan/updated" => {
                let mut text = params["explanation"].as_str().unwrap_or("").to_owned();
                if let Some(plan) = params["plan"].as_array() {
                    for step in plan {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        let marker = match step["status"].as_str().unwrap_or("") {
                            "completed" => "[x]",
                            "inProgress" => "[>]",
                            _ => "[ ]",
                        };
                        text.push_str(&format!("{marker} {}", step["step"].as_str().unwrap_or("")));
                    }
                }
                let message = self.native_message(
                    format!("plan:{}", params["turnId"]),
                    "PLAN",
                    "plan".into(),
                );
                message.replace_text(text);
                message.inspection = Some(InspectionScope::TurnMethod {
                    turn_id: params["turnId"].as_str().unwrap_or("").into(),
                    method: "turn/plan/updated",
                });
            }
            "turn/diff/updated" => {
                let message = self.native_message(
                    format!("diff:{}", params["turnId"]),
                    "DIFF",
                    "changes".into(),
                );
                message.replace_text(params["diff"].as_str().unwrap_or("").into());
                message.inspection = Some(InspectionScope::TurnMethod {
                    turn_id: params["turnId"].as_str().unwrap_or("").into(),
                    method: "turn/diff/updated",
                });
            }
            "thread/tokenUsage/updated" => {
                self.usage.update(&params);
            }
            "item/agentMessage/delta"
            | "turn/started"
            | "turn/completed"
            | "serverRequest/resolved"
            | "thread/status/changed"
            | "thread/name/updated" => {}
            _ => {
                let title = method.clone();
                let mut message = Message::text("EVENT", pretty(&params));
                message.title = title;
                message.inspection = Some(InspectionScope::Event(self.native.len()));
                self.messages.push(message);
            }
        }
        self.native.push(NativeRecord { method, params });
    }
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

fn merge_snapshot(stream: &str, snapshot: &str) -> String {
    if stream.is_empty() || snapshot.contains(stream) {
        snapshot.into()
    } else if snapshot.is_empty() || stream.contains(snapshot) {
        stream.into()
    } else {
        format!("{stream}\n{snapshot}")
    }
}

fn describe_item(item: &Value) -> (&'static str, String, String) {
    let kind = item["type"].as_str().unwrap_or("item");
    match kind {
        "commandExecution" => {
            let command = item["command"].as_str().unwrap_or("command");
            let status = item["status"].as_str().unwrap_or("");
            let exit = item["exitCode"]
                .as_i64()
                .map(|n| format!(" · exit {n}"))
                .unwrap_or_default();
            (
                "TOOL",
                format!("{command}  {status}{exit}"),
                item["aggregatedOutput"].as_str().unwrap_or("").into(),
            )
        }
        "reasoning" => {
            let mut parts = Vec::new();
            for field in ["summary", "content"] {
                if let Some(items) = item[field].as_array() {
                    for value in items {
                        if let Some(text) = value.as_str().or_else(|| value["text"].as_str()) {
                            parts.push(text);
                        }
                    }
                }
            }
            ("THOUGHT", "reasoning".into(), parts.join("\n"))
        }
        "fileChange" => {
            let text = item["changes"]
                .as_array()
                .map(|changes| {
                    changes
                        .iter()
                        .map(|change| {
                            format!(
                                "{}\n{}",
                                change["path"].as_str().unwrap_or("file"),
                                change["diff"].as_str().unwrap_or("")
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_else(|| pretty(item));
            (
                "DIFF",
                format!("files · {}", item["status"].as_str().unwrap_or("")),
                text,
            )
        }
        "webSearch" => ("TOOL", "web search".into(), pretty(&item["action"])),
        "mcpToolCall" => (
            "TOOL",
            format!(
                "{}.{}",
                item["server"].as_str().unwrap_or("mcp"),
                item["tool"].as_str().unwrap_or("tool")
            ),
            pretty(item),
        ),
        _ => ("TOOL", kind.into(), pretty(item)),
    }
}

pub(crate) struct ClientApp {
    runtime: Runtime,
    worker: Option<Worker>,
    receiver: Option<mpsc::UnboundedReceiver<ServerEvent>>,
    commands: Option<mpsc::UnboundedSender<ClientCommand>>,
    address: String,
    pairing: String,
    reveal_pairing: bool,
    connected: bool,
    stopping: bool,
    workspace: String,
    sessions: Vec<Session>,
    selected: Option<String>,
    models: Vec<ModelInfo>,
    default_model: Option<String>,
    account_usage: Option<AccountUsage>,
    new_name: String,
    new_model: Option<String>,
    new_effort: Option<String>,
    new_workspace: String,
    show_new: bool,
    show_sidebar: bool,
    new_pending: bool,
    error: Option<String>,
    close_requested: bool,
    demo: bool,
}

impl ClientApp {
    pub(crate) fn new(demo: bool) -> Self {
        let mut app = Self {
            runtime: runtime(),
            worker: None,
            receiver: None,
            commands: None,
            address: "127.0.0.1:7331".into(),
            pairing: String::new(),
            reveal_pairing: false,
            connected: false,
            stopping: false,
            workspace: String::new(),
            sessions: vec![],
            selected: None,
            models: vec![],
            default_model: None,
            account_usage: None,
            new_name: String::new(),
            new_model: None,
            new_effort: None,
            new_workspace: String::new(),
            show_new: false,
            show_sidebar: true,
            new_pending: false,
            error: None,
            close_requested: false,
            demo,
        };
        if demo {
            app.connected = true;
            app.address = "192.168.1.42:7331".into();
            app.workspace = "/data/scratch".into();
            let mut session = Session::new(
                "demo-01".into(),
                "Optics datasheet".into(),
                "/data/scratch/codex-lan-threads/session-optics-demo".into(),
            );
            session.model = Some("gpt-6.1-sol".into());
            session.effort = Some("medium".into());
            session.messages.push(Message::text(
                "USER",
                "Download the optics datasheet and tell me where you put it.",
            ));
            session.agent_text(
                "demo-a".into(),
                "I’ll download the PDF into this session’s directory.".into(),
                true,
            );
            session.native_event("item/completed".into(), json!({"item":{"id":"demo-a","type":"agentMessage","text":"I’ll download the PDF into this session’s directory."}}));
            session.native_event(
                "item/started".into(),
                json!({"item":{"id":"demo-metadata","type":"reasoning"}}),
            );
            session.native_event(
                "item/completed".into(),
                json!({"item":{"id":"demo-metadata","type":"reasoning","summary":[],"content":[]}}),
            );
            session.native_event("turn/plan/updated".into(), json!({"turnId":"demo-turn","plan":[{"step":"Download the datasheet","status":"completed"},{"step":"Verify the file and report its path","status":"completed"}]}));
            session.native_event("item/started".into(), json!({"item":{"id":"demo-command","type":"commandExecution","command":"curl -L --fail -o datasheet.pdf <document-url>","status":"inProgress"}}));
            session.native_event("item/commandExecution/outputDelta".into(), json!({"itemId":"demo-command","delta":"100  108k  100  108k    0     0   238k      0\n"}));
            session.native_event("item/completed".into(), json!({"item":{"id":"demo-command","type":"commandExecution","command":"curl -L --fail -o datasheet.pdf <document-url>","status":"completed","exitCode":0,"aggregatedOutput":"100  108k  100  108k    0     0   238k      0\n"}}));
            session.native_event("item/reasoning/summaryTextDelta".into(), json!({"itemId":"demo-check","delta":"Checking the saved file before reporting its path."}));
            session.native_event("document/inspected".into(), json!({"file":"datasheet.pdf","bytes":111000,"properties":{"format":"PDF","verified":true,"warnings":[],"metadata":null}}));
            session.agent_text("demo-final".into(), "Saved the PDF (111 KB):\n/data/scratch/codex-lan-threads/session-optics-demo/datasheet.pdf".into(), true);
            session.native_event("item/completed".into(), json!({"item":{"id":"demo-final","type":"agentMessage","text":"Saved the PDF (111 KB):\n/data/scratch/codex-lan-threads/session-optics-demo/datasheet.pdf"}}));
            session.native_event("thread/tokenUsage/updated".into(), json!({"tokenUsage":{"total":{"inputTokens":48000,"cachedInputTokens":40000,"outputTokens":3400,"reasoningOutputTokens":2800,"totalTokens":51400}}}));
            app.account_usage = Some(
                serde_json::from_value(json!({
                    "limits":[{"id":"codex","name":"Codex","model":null,"plan":"pro",
                        "primary":{"used_percent":12,"window_minutes":300,"resets_at":null},
                        "secondary":{"used_percent":24,"window_minutes":10080,"resets_at":null},
                        "spend_control_reached":false}],
                    "ordinary_usage_allowed":true,"unavailable_reason":null
                }))
                .expect("valid demo usage"),
            );
            // Only the isolated --demo preview reads this; live sessions always
            // begin in Replies and change through the per-session selector.
            if let Ok(level) = std::env::var("CODEX_LAN_DEMO_LEVEL")
                && let Some(selected) = OutputLevel::ALL
                    .into_iter()
                    .find(|candidate| candidate.label().eq_ignore_ascii_case(level.trim()))
            {
                session.output_level = selected;
            }
            session.activity = "completed".into();
            app.selected = Some(session.id.clone());
            app.sessions.push(session);
            app.sessions.push(Session::new(
                "demo-02".into(),
                "Review the build".into(),
                "/data/scratch/codex-lan-threads/session-build-demo".into(),
            ));
        }
        app
    }

    fn connect(&mut self) {
        self.error = None;
        let config = ClientConfig {
            address: self.address.trim().into(),
            pairing_code: self.pairing.trim().into(),
        };
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (done_tx, done_rx) = oneshot::channel();
        let stop = CancellationToken::new();
        let task_stop = stop.clone();
        self.runtime.spawn(async move {
            let result = transport::run_client(config, command_rx, event_tx, task_stop).await;
            let _ = done_tx.send(result.map_err(|e| format!("{e:#}")));
        });
        self.commands = Some(command_tx);
        self.receiver = Some(event_rx);
        self.worker = Some(Worker {
            stop,
            done: done_rx,
        });
        self.stopping = false;
    }

    fn send(&mut self, command: ClientCommand) -> bool {
        match &self.commands {
            Some(sender) if sender.send(command).is_ok() => true,
            _ => {
                self.error = Some("Connection is unavailable.".into());
                false
            }
        }
    }

    fn close_session(&mut self, session_id: &str) {
        if self.demo || self.stopping || !self.connected {
            return;
        }
        let Some(index) = self
            .sessions
            .iter()
            .position(|session| session.id == session_id && !session.closing)
        else {
            return;
        };
        if self.send(ClientCommand::CloseSession {
            session_id: session_id.into(),
        }) {
            self.sessions[index].closing = true;
        }
    }

    fn drain(&mut self) {
        let mut events = Vec::new();
        if let Some(receiver) = &mut self.receiver {
            for _ in 0..4096 {
                match receiver.try_recv() {
                    Ok(event) => events.push(event),
                    Err(_) => break,
                }
            }
        }
        for event in events {
            self.handle(event);
        }
        let finished = self
            .worker
            .as_mut()
            .and_then(|worker| match worker.done.try_recv() {
                Ok(result) => Some(result),
                Err(oneshot::error::TryRecvError::Closed) => {
                    Some(Err("Connection task stopped unexpectedly.".into()))
                }
                Err(oneshot::error::TryRecvError::Empty) => None,
            });
        if let Some(result) = finished {
            if let Err(error) = result {
                self.error = Some(error);
            }
            self.worker = None;
            self.receiver = None;
            self.commands = None;
            self.connected = false;
            self.stopping = false;
            self.new_pending = false;
            self.sessions.clear();
            self.selected = None;
            self.workspace.clear();
            self.models.clear();
            self.default_model = None;
            self.account_usage = None;
        }
    }

    fn handle(&mut self, event: ServerEvent) {
        match event {
            ServerEvent::AccountUsage { usage } => self.account_usage = Some(usage),
            ServerEvent::Welcome { workspace, .. } => {
                self.connected = true;
                self.workspace = workspace;
                if !self.demo {
                    self.send(ClientCommand::ListModels);
                }
            }
            ServerEvent::Models {
                models,
                default_model,
            } => {
                self.models = models;
                self.default_model = default_model;
            }
            ServerEvent::SessionStarted {
                session_id,
                name,
                workspace,
                model,
                effort,
            } => {
                self.new_pending = false;
                self.show_new = false;
                if !self.sessions.iter().any(|session| session.id == session_id) {
                    self.selected = Some(session_id.clone());
                    let mut session = Session::new(session_id, name, workspace);
                    session.model = model;
                    session.effort = effort;
                    self.sessions.push(session);
                }
            }
            ServerEvent::SessionClosed { session_id } => {
                self.sessions.retain(|session| session.id != session_id);
                if self.selected.as_ref() == Some(&session_id) {
                    self.selected = self.sessions.first().map(|s| s.id.clone());
                }
            }
            ServerEvent::Error {
                session_id,
                message,
            } => {
                self.new_pending = false;
                if let Some(session) = self
                    .sessions
                    .iter_mut()
                    .find(|s| Some(&s.id) == session_id.as_ref())
                {
                    session.closing = false;
                    session.messages.push(Message::text("ERROR", message));
                } else {
                    self.error = Some(message);
                }
            }
            event => {
                let Some(session) = self
                    .sessions
                    .iter_mut()
                    .find(|s| Some(s.id.as_str()) == event.session_id())
                else {
                    return;
                };
                match event {
                    ServerEvent::Delta { item_id, text, .. } => {
                        session.agent_text(item_id, text, false)
                    }
                    ServerEvent::ItemCompleted { item_id, text, .. } => {
                        session.agent_text(item_id, text, true)
                    }
                    ServerEvent::NativeEvent { method, params, .. } => {
                        session.native_event(method, params)
                    }
                    ServerEvent::PromptAccepted { steered, .. } => {
                        session.pending_prompts = session.pending_prompts.saturating_sub(1);
                        session.activity = if steered {
                            "steering accepted"
                        } else {
                            "prompt accepted"
                        }
                        .into();
                    }
                    ServerEvent::PromptRejected { text, message, .. } => {
                        session.pending_prompts = session.pending_prompts.saturating_sub(1);
                        session.messages.push(Message::text(
                            "ERROR",
                            format!("Prompt not accepted: {message}\n\n{text}"),
                        ));
                        if session.draft.is_empty() {
                            session.draft = text;
                        }
                    }
                    ServerEvent::TurnStarted { .. } => {
                        session.active = true;
                        session.activity = "working".into();
                    }
                    ServerEvent::TurnFinished { status, .. } => {
                        session.active = false;
                        session.activity = status;
                        session.requests.clear();
                    }
                    ServerEvent::Activity { text, .. } => session.activity = text,
                    ServerEvent::Request {
                        request_id,
                        method,
                        params,
                        ..
                    } => {
                        if !session.requests.iter().any(|r| r.id == request_id) {
                            session.requests.push(Request {
                                id: request_id,
                                method,
                                params,
                                answers: BTreeMap::new(),
                                raw: "{}".into(),
                                error: None,
                            });
                        }
                    }
                    ServerEvent::RequestResolved { request_id, .. } => {
                        session.requests.retain(|r| r.id != request_id)
                    }
                    _ => {}
                }
            }
        }
    }
}

impl Drop for ClientApp {
    fn drop(&mut self) {
        if let Some(worker) = &self.worker {
            worker.stop.cancel();
        }
    }
}

#[cfg(test)]
mod ui_checks;

#[cfg(test)]
mod tests;
