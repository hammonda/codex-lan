//! Bounded, replayable output. Reading a cursor never consumes another reader's data.
use anyhow::{Context, Result, bail, ensure};
use codex_lan_core::{protocol::ServerEvent, usage::ThreadUsage};
use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet, VecDeque};

const MAX_HISTORY_BYTES: usize = 64 * 1024 * 1024;
const MAX_RECORDS: usize = 20_000;

#[derive(
    Clone, Copy, Default, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq, PartialOrd, Ord,
)]
#[serde(rename_all = "snake_case")]
pub enum Detail {
    #[default]
    Replies,
    Progress,
    Tools,
    Everything,
}

struct Record {
    sequence: u64,
    level: Detail,
    kind: &'static str,
    text: String,
}

#[derive(Default)]
struct TurnOutput {
    final_reply: Option<String>,
    // Older native versions omit phase. Only their last completed message is returned.
    last_reply: Option<String>,
}

pub struct Thread {
    pub id: String,
    name: String,
    workspace: String,
    model: Option<String>,
    effort: Option<String>,
    pub status: String,
    pub submissions: HashSet<String>,
    accepted_turn: Option<String>,
    turns: VecDeque<(String, String)>,
    pub requests: BTreeMap<String, Value>,
    outputs: VecDeque<(String, TurnOutput)>,
    last_error: Option<String>,
    usage: ThreadUsage,
    records: VecDeque<Record>,
    bytes: usize,
    next: u64,
}

impl Thread {
    pub fn started(event: ServerEvent) -> Self {
        let ServerEvent::SessionStarted {
            session_id,
            name,
            workspace,
            model,
            effort,
        } = event
        else {
            unreachable!("only a session-started event constructs a thread")
        };
        Self {
            id: session_id,
            name,
            workspace,
            model,
            effort,
            status: "idle".into(),
            submissions: HashSet::new(),
            accepted_turn: None,
            turns: VecDeque::new(),
            requests: BTreeMap::new(),
            outputs: VecDeque::new(),
            last_error: None,
            usage: ThreadUsage::default(),
            records: VecDeque::new(),
            bytes: 0,
            next: 1,
        }
    }

    pub fn summary(&self) -> Value {
        json!({
            "thread_id": self.id, "name": self.name, "workspace": self.workspace,
            "model": self.model, "effort": self.effort, "status": self.current_status(),
            "active": self.active(), "turn_id":self.accepted_turn, "usage": self.usage.snapshot(self.model.as_deref()),
            "pending_requests": self.requests.iter().map(|(id, request)| json!({"request_id":id,"method":request["method"]})).collect::<Vec<_>>(),
            "earliest_cursor": format!("{}:0", self.first()),
            "latest_cursor": format!("{}:0", self.next),
            "history_truncated": self.first() > 1,
        })
    }

    pub fn current_status(&self) -> &str {
        if matches!(self.status.as_str(), "disconnected" | "backend_failed") {
            return &self.status;
        }
        if !self.submissions.is_empty() {
            return "submitting";
        }
        if !self.requests.is_empty() {
            return "needs_input";
        }
        if let Some(id) = &self.accepted_turn {
            return self
                .turns
                .iter()
                .find(|(turn, _)| turn == id)
                .map_or("starting", |(_, status)| status);
        }
        &self.status
    }

    pub fn active(&self) -> bool {
        matches!(
            self.current_status(),
            "submitting" | "starting" | "working" | "needs_input"
        )
    }

    pub fn brief(&self) -> Value {
        json!({"thread_id":self.id,"name":self.name,"status":self.current_status()})
    }

    /// A model-facing outcome, separate from the lossless diagnostic transcript.
    pub fn outcome(&self) -> Value {
        let status = self.current_status();
        let mut value = json!({"thread_id":self.id,"status":status});
        if status == "needs_input" {
            value["requests"] = json!(self.requests.values().collect::<Vec<_>>());
        } else if !self.active() {
            if let Some((_, output)) = self
                .outputs
                .iter()
                .find(|(id, _)| Some(id) == self.accepted_turn.as_ref())
                && let Some(reply) = output.final_reply.as_ref().or(output.last_reply.as_ref())
            {
                value["reply"] = json!(reply);
            }
            if status != "completed"
                && let Some(error) = &self.last_error
            {
                value["error"] = json!(error);
            }
        }
        value
    }

    fn collect_reply(&mut self, turn: &str, item: &Value) {
        if item["type"] != "agentMessage" {
            return;
        }
        let Some(text) = item["text"].as_str() else {
            return;
        };
        if !self.outputs.iter().any(|(id, _)| id == turn) {
            self.outputs.push_back((turn.into(), TurnOutput::default()));
            while self.outputs.len() > 4 {
                self.outputs.pop_front();
            }
        }
        let output = &mut self
            .outputs
            .iter_mut()
            .find(|(id, _)| id == turn)
            .unwrap()
            .1;
        match item["phase"].as_str() {
            Some("final_answer") => output.final_reply = Some(text.into()),
            None => output.last_reply = Some(text.into()),
            _ => {} // Commentary/reasoning stays in the human trace, never the result.
        }
    }

    fn turn_status(&mut self, id: String, status: String) {
        if let Some((_, old)) = self.turns.iter_mut().find(|(turn, _)| turn == &id) {
            *old = status;
        } else {
            self.turns.push_back((id, status));
        }
        while self.turns.len() > 128 {
            self.turns.pop_front();
        }
    }

    fn first(&self) -> u64 {
        self.records
            .front()
            .map_or(self.next, |record| record.sequence)
    }

    fn push(&mut self, level: Detail, kind: &'static str, text: String) {
        self.bytes += text.len();
        self.records.push_back(Record {
            sequence: self.next,
            level,
            kind,
            text,
        });
        self.next += 1;
        while self.bytes > MAX_HISTORY_BYTES || self.records.len() > MAX_RECORDS {
            if let Some(old) = self.records.pop_front() {
                self.bytes -= old.text.len();
            }
        }
    }

    pub fn event(&mut self, event: ServerEvent) {
        match event {
            ServerEvent::ItemCompleted { item_id, text, .. } => {
                self.push(
                    Detail::Replies,
                    "reply",
                    json!({"item_id":item_id,"text":text}).to_string(),
                );
            }
            ServerEvent::Delta { item_id, text, .. } => {
                self.push(
                    Detail::Progress,
                    "reply_delta",
                    json!({"item_id":item_id,"text":text}).to_string(),
                );
            }
            ServerEvent::PromptAccepted { turn_id, .. } => {
                self.accepted_turn = Some(turn_id);
            }
            ServerEvent::TurnStarted { turn_id, .. } => {
                self.status = "working".into();
                self.last_error = None;
                self.turn_status(turn_id, "working".into());
                self.push(Detail::Replies, "status", "working".into());
            }
            ServerEvent::TurnFinished {
                turn_id, status, ..
            } => {
                self.turn_status(turn_id, status.clone());
                self.requests.clear();
                self.status = status.clone();
                self.push(Detail::Replies, "status", status);
            }
            ServerEvent::Request {
                request_id,
                method,
                params,
                ..
            } => {
                self.requests.insert(
                    request_id.clone(),
                    json!({"request_id":request_id,"method":method,"params":params}),
                );
                self.status = "needs_input".into();
                self.push(
                    Detail::Replies,
                    "request",
                    json!({"request_id":request_id,"method":method,"params":params}).to_string(),
                );
            }
            ServerEvent::RequestResolved { request_id, .. } => {
                self.requests.remove(&request_id);
                if self.requests.is_empty() {
                    self.status = "working".into();
                }
                self.push(Detail::Replies, "request_resolved", request_id);
            }
            ServerEvent::Activity { text, .. } => self.push(Detail::Progress, "activity", text),
            ServerEvent::Error { message, .. } | ServerEvent::PromptRejected { message, .. } => {
                self.last_error = Some(message.clone());
                self.push(Detail::Replies, "error", message);
            }
            ServerEvent::NativeEvent { method, params, .. } => {
                if method == "item/completed"
                    && let Some(turn) = params["turnId"].as_str()
                {
                    self.collect_reply(turn, &params["item"]);
                }
                if method == "turn/completed"
                    && let Some(turn) = params.pointer("/turn/id").and_then(Value::as_str)
                    && let Some(items) = params.pointer("/turn/items").and_then(Value::as_array)
                {
                    for item in items {
                        self.collect_reply(turn, item);
                    }
                }
                if method == "thread/tokenUsage/updated" {
                    self.usage.update(&params);
                }
                let level = native_level(&method, &params);
                self.push(
                    level,
                    "native",
                    json!({"method":method,"params":params}).to_string(),
                );
            }
            _ => {}
        }
    }

    pub fn disconnected(&mut self, message: &str) {
        self.submissions.clear();
        self.status = "disconnected".into();
        self.last_error = Some(message.into());
        self.push(Detail::Replies, "error", message.into());
    }

    pub fn page(&self, cursor: Option<&str>, detail: Detail, max_bytes: usize) -> Result<Value> {
        let (mut sequence, mut offset) = match cursor {
            None => (self.first(), 0),
            Some(cursor) => {
                let (sequence, offset) = cursor
                    .split_once(':')
                    .context("Invalid cursor; use next_cursor from thread_read")?;
                (
                    sequence.parse::<u64>().context("Invalid cursor sequence")?,
                    offset.parse::<usize>().context("Invalid cursor offset")?,
                )
            }
        };
        if sequence < self.first() {
            bail!(
                "Output history expired. Earliest available cursor is {}:0; read without a cursor to inspect retained output",
                self.first()
            );
        }
        ensure!(
            sequence <= self.next,
            "Cursor is ahead of this thread's output"
        );
        ensure!(
            sequence != self.next || offset == 0,
            "Invalid cursor offset"
        );
        let mut remaining = max_bytes;
        let mut events = Vec::new();
        let start = sequence;
        for record in self
            .records
            .iter()
            .filter(|record| record.sequence >= start)
        {
            ensure!(
                offset <= record.text.len() && record.text.is_char_boundary(offset),
                "Invalid cursor offset"
            );
            if record.level > detail || offset == record.text.len() {
                sequence = record.sequence + 1;
                offset = 0;
                continue;
            }
            if remaining < 4 || events.len() >= 32 {
                break;
            }
            let mut end = (offset + remaining).min(record.text.len());
            while !record.text.is_char_boundary(end) {
                end -= 1;
            }
            let complete = end == record.text.len();
            events.push(json!({
                "sequence":record.sequence, "kind":record.kind, "offset":offset,
                "text":&record.text[offset..end], "complete":complete,
            }));
            remaining -= end - offset;
            if complete {
                sequence = record.sequence + 1;
                offset = 0;
            } else {
                sequence = record.sequence;
                offset = end;
                break;
            }
        }
        Ok(json!({
            "thread": self.summary(), "events": events,
            "next_cursor": format!("{sequence}:{offset}"),
            "has_more": sequence < self.next,
        }))
    }
}

fn native_level(method: &str, params: &Value) -> Detail {
    let item_type = params
        .pointer("/item/type")
        .and_then(Value::as_str)
        .unwrap_or("");
    if method.contains("reasoning")
        || method == "turn/plan/updated"
        || item_type == "reasoning"
        || item_type == "plan"
    {
        Detail::Progress
    } else if method.contains("commandExecution")
        || method.contains("fileChange")
        || method == "turn/diff/updated"
        || matches!(
            item_type,
            "commandExecution" | "fileChange" | "mcpToolCall" | "webSearch" | "dynamicToolCall"
        )
    {
        Detail::Tools
    } else {
        Detail::Everything
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thread() -> Thread {
        Thread::started(ServerEvent::SessionStarted {
            session_id: "one".into(),
            name: "Task".into(),
            workspace: "/prepared".into(),
            model: None,
            effort: None,
        })
    }

    #[test]
    fn unicode_pages_reassemble_exactly_and_reads_are_replayable() {
        let mut thread = thread();
        let original = "é🙂\n\"".repeat(1000);
        thread.push(Detail::Replies, "reply", original.clone());
        let mut reconstructed = String::new();
        let mut cursor = None;
        loop {
            let page = thread
                .page(cursor.as_deref(), Detail::Replies, 256)
                .unwrap();
            assert_eq!(
                page,
                thread
                    .page(cursor.as_deref(), Detail::Replies, 256)
                    .unwrap()
            );
            let record = &page["events"][0];
            assert_eq!(
                record["offset"].as_u64().unwrap() as usize,
                reconstructed.len()
            );
            reconstructed.push_str(record["text"].as_str().unwrap());
            cursor = Some(page["next_cursor"].as_str().unwrap().to_owned());
            if !page["has_more"].as_bool().unwrap() {
                break;
            }
        }
        assert_eq!(reconstructed, original);
        assert!(thread.page(Some("1:1"), Detail::Replies, 256).is_err());
        assert!(thread.page(Some("999:0"), Detail::Replies, 256).is_err());
        assert!(thread.page(Some("garbage"), Detail::Replies, 256).is_err());
    }

    #[test]
    fn changing_detail_reveals_retained_events_and_expired_cursors_are_explicit() {
        let mut thread = thread();
        thread.event(ServerEvent::NativeEvent {
            session_id: "one".into(),
            method: "fixture/new-event".into(),
            params: json!({"keep":"all fields"}),
        });
        thread.push(Detail::Replies, "reply", "answer".into());
        let replies = thread.page(None, Detail::Replies, 8000).unwrap();
        assert_eq!(replies["events"].as_array().unwrap().len(), 1);
        assert_eq!(replies["events"][0]["text"], "answer");
        assert_eq!(
            thread.page(None, Detail::Everything, 8000).unwrap()["events"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        for _ in 0..MAX_RECORDS {
            thread.push(Detail::Progress, "activity", "working".into());
        }
        assert!(
            thread
                .page(Some("1:0"), Detail::Replies, 8000)
                .unwrap_err()
                .to_string()
                .contains("expired")
        );
        let retained = thread.page(None, Detail::Replies, 8000).unwrap();
        assert_eq!(retained["thread"]["history_truncated"], true);
        assert_eq!(retained["has_more"], false);
    }

    #[test]
    fn accepted_turn_cannot_be_confused_with_previous_completion_or_late_ack() {
        let mut thread = thread();
        thread.event(ServerEvent::PromptAccepted {
            session_id: "one".into(),
            turn_id: "new".into(),
            steered: false,
        });
        thread.event(ServerEvent::TurnFinished {
            session_id: "one".into(),
            turn_id: "old".into(),
            status: "completed".into(),
        });
        assert_eq!(thread.summary()["status"], "starting");
        assert!(thread.active());
        thread.event(ServerEvent::TurnFinished {
            session_id: "one".into(),
            turn_id: "new".into(),
            status: "completed".into(),
        });
        thread.event(ServerEvent::PromptAccepted {
            session_id: "one".into(),
            turn_id: "new".into(),
            steered: false,
        });
        assert_eq!(thread.summary()["status"], "completed");
        assert!(!thread.active());
        thread.submissions.insert("pending".into());
        assert_eq!(thread.summary()["status"], "submitting");
        assert!(thread.active());
    }

    #[test]
    fn final_result_survives_transcript_eviction_and_uses_the_accepted_turn() {
        let mut thread = thread();
        for (turn, text) in [("old", "Old answer"), ("new", "New answer")] {
            for (id, phase, text) in [
                ("progress", Some("commentary"), "Human progress"),
                ("answer", None, text),
            ] {
                thread.event(ServerEvent::NativeEvent { session_id: "one".into(), method: "item/completed".into(), params: json!({"turnId":turn,"item":{"id":id,"type":"agentMessage","phase":phase,"text":text}}) });
            }
            thread.event(ServerEvent::TurnFinished {
                session_id: "one".into(),
                turn_id: turn.into(),
                status: "completed".into(),
            });
        }
        thread.event(ServerEvent::PromptAccepted {
            session_id: "one".into(),
            turn_id: "new".into(),
            steered: false,
        });
        for _ in 0..MAX_RECORDS {
            thread.push(Detail::Tools, "activity", "tool detail".into());
        }
        assert_eq!(thread.outcome()["reply"], "New answer");
        assert_eq!(thread.outcome()["status"], "completed");
        assert!(thread.page(Some("1:0"), Detail::Replies, 8000).is_err());
        thread.event(ServerEvent::PromptAccepted {
            session_id: "one".into(),
            turn_id: "next".into(),
            steered: false,
        });
        assert!(thread.active());
        assert!(thread.outcome().get("reply").is_none());
    }

    #[test]
    fn completion_snapshot_can_supply_final_answer_without_replaying_commentary() {
        let mut thread = thread();
        thread.event(ServerEvent::NativeEvent { session_id: "one".into(), method: "turn/completed".into(), params: json!({"turn":{"id":"turn","items":[{"type":"agentMessage","phase":"final_answer","text":"Final"},{"type":"agentMessage","phase":"commentary","text":"Trailing progress"}]}}) });
        thread.event(ServerEvent::TurnFinished {
            session_id: "one".into(),
            turn_id: "turn".into(),
            status: "completed".into(),
        });
        thread.event(ServerEvent::PromptAccepted {
            session_id: "one".into(),
            turn_id: "turn".into(),
            steered: false,
        });
        assert_eq!(thread.outcome()["reply"], "Final");
    }
}
