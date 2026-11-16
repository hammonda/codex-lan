//! A chronological, read-only view of traffic through one server session.
mod demo;
#[cfg(test)]
mod tests;
mod view;

use super::*;
use std::time::Instant;

const MAX_BYTES: usize = 8 * 1024 * 1024;
const MAX_EVENTS: usize = 5_000;

#[derive(Clone, Copy)]
struct Stamp {
    sequence: u64,
    elapsed: Duration,
    incoming: bool,
}

struct FlowRow {
    stamp: Stamp,
    key: Option<String>,
    message: Message,
    records: Vec<usize>,
}

#[derive(Default)]
struct Stream {
    text: String,
    segments: usize,
}

pub(crate) struct SessionObserver {
    // Parse one native event at a time; item upserts must not rewrite history.
    session: Session,
    rows: Vec<FlowRow>,
    stamps: Vec<Stamp>,
    streams: BTreeMap<String, Stream>,
    client: &'static str,
    started: Instant,
    sequence: u64,
    bytes: usize,
    events: usize,
    discarded: bool,
    at_bottom: bool,
}

impl SessionObserver {
    pub(crate) fn new(id: String, name: String, workspace: String) -> Self {
        Self {
            client: if name.starts_with("Claude /") {
                "Claude"
            } else {
                "Client"
            },
            session: Session::new(id, name, workspace),
            rows: vec![],
            stamps: vec![],
            streams: BTreeMap::new(),
            started: Instant::now(),
            sequence: 0,
            bytes: 0,
            events: 0,
            discarded: false,
            at_bottom: true,
        }
    }

    fn retain(&mut self, bytes: usize) -> bool {
        if self.bytes + bytes > MAX_BYTES || self.events >= MAX_EVENTS {
            self.session.messages.clear();
            self.session.native.clear();
            self.rows.clear();
            self.stamps.clear();
            self.streams.clear();
            self.bytes = 0;
            self.events = 0;
            self.discarded = true;
        }
        if bytes > MAX_BYTES {
            return false;
        }
        self.bytes += bytes;
        self.events += 1;
        true
    }

    fn record(&mut self, incoming: bool, method: String, params: Value) -> usize {
        self.sequence += 1;
        self.stamps.push(Stamp {
            sequence: self.sequence,
            elapsed: self.started.elapsed(),
            incoming,
        });
        self.session.native.push(NativeRecord { method, params });
        self.session.native.len() - 1
    }

    fn push(
        &mut self,
        index: usize,
        key: Option<String>,
        role: &'static str,
        title: String,
        text: String,
    ) {
        let mut message = Message::text(role, text);
        message.title = title;
        message.expanded = role != "EVENT";
        self.rows.push(FlowRow {
            stamp: self.stamps[index],
            key,
            message,
            records: vec![index],
        });
    }

    fn stream(
        &mut self,
        index: usize,
        key: String,
        role: &'static str,
        title: String,
        text: String,
        snapshot: bool,
    ) {
        let stream = self.streams.entry(key.clone()).or_default();
        let mut replace = false;
        let mut revision = false;
        let tail = if snapshot {
            if let Some(tail) = text.strip_prefix(&stream.text) {
                tail.to_owned()
            } else if stream.text.starts_with(&text) {
                String::new()
            } else {
                // Corrected snapshots can replace one uninterrupted row.
                // Across interleaving, show them at their new position.
                replace = stream.segments == 1
                    && self
                        .rows
                        .last()
                        .is_some_and(|row| row.key.as_ref() == Some(&key));
                revision = !replace;
                text.clone()
            }
        } else {
            text.clone()
        };
        if snapshot {
            if !stream.text.starts_with(&text) {
                stream.text = text;
            }
        } else {
            stream.text.push_str(&text);
        }
        if revision {
            stream.segments += 1;
            self.push(
                index,
                Some(key),
                role,
                format!("{title} · revised snapshot"),
                tail,
            );
            return;
        }
        if tail.is_empty() {
            if let Some(row) = self
                .rows
                .iter_mut()
                .rev()
                .find(|row| row.key.as_ref() == Some(&key))
            {
                row.records.push(index);
            }
            return;
        }
        if let Some(row) = self
            .rows
            .last_mut()
            .filter(|row| row.key.as_ref() == Some(&key))
        {
            if replace {
                row.message.replace_text(tail);
            } else {
                row.message.append_text(&tail);
            }
            row.records.push(index);
        } else {
            let title = if stream.segments > 0 {
                format!("{title} · continued")
            } else {
                title
            };
            stream.segments += 1;
            self.push(index, Some(key), role, title, tail);
        }
    }

    pub(crate) fn command(&mut self, command: ClientCommand) {
        if !self.retain(serde_json::to_vec(&command).map_or(0, |bytes| bytes.len())) {
            return;
        }
        let params = serde_json::to_value(&command).expect("Client command serialization");
        let index = self.record(
            true,
            format!("client/{}", params["type"].as_str().unwrap_or("command")),
            params,
        );
        let (title, text) = match command {
            ClientCommand::Prompt { text, .. } => ("prompt", text),
            ClientCommand::Reply {
                request_id, result, ..
            } => (
                "answer",
                pretty(&json!({"reply_to":request_id,"result":result})),
            ),
            ClientCommand::Interrupt { .. } => ("interrupt", "Interrupt requested".into()),
            ClientCommand::CloseSession { .. } => ("close", "Close requested".into()),
            _ => return,
        };
        self.push(index, None, "USER", title.into(), text);
    }

    pub(crate) fn event(&mut self, event: ServerEvent) {
        if !self.retain(serde_json::to_vec(&event).map_or(0, |bytes| bytes.len())) {
            return;
        }
        if let ServerEvent::NativeEvent { method, params, .. } = event {
            self.native_event(method, params);
            return;
        }
        let params = serde_json::to_value(&event).expect("Server event serialization");
        let index = self.record(
            false,
            format!("bridge/{}", params["type"].as_str().unwrap_or("event")),
            params,
        );
        match event {
            ServerEvent::Delta { item_id, text, .. } => {
                self.stream(index, item_id, "AGENT", "reply".into(), text, false)
            }
            ServerEvent::ItemCompleted { item_id, text, .. } => {
                self.stream(index, item_id, "AGENT", "reply".into(), text, true)
            }
            ServerEvent::Error { message, .. } | ServerEvent::PromptRejected { message, .. } => {
                self.push(index, None, "ERROR", "error".into(), message)
            }
            ServerEvent::Request {
                request_id,
                method,
                params,
                ..
            } => {
                self.session.activity = "awaiting input".into();
                self.push(
                    index,
                    None,
                    "AGENT",
                    "question".into(),
                    pretty(&json!({"request_id":request_id,"method":method,"params":params})),
                );
            }
            ServerEvent::TurnStarted { .. } => self.session.activity = "working".into(),
            ServerEvent::TurnFinished { status, .. } => self.session.activity = status,
            ServerEvent::Activity { text, .. } => self.session.activity = text,
            ServerEvent::PromptAccepted { steered, .. } => {
                self.session.activity = if steered {
                    "steering accepted"
                } else {
                    "prompt accepted"
                }
                .into()
            }
            _ => {}
        }
    }

    fn native_event(&mut self, method: String, params: Value) {
        self.sequence += 1;
        let index = self.session.native.len();
        self.stamps.push(Stamp {
            sequence: self.sequence,
            elapsed: self.started.elapsed(),
            incoming: false,
        });
        self.session.native_event(method.clone(), params.clone());
        let Some(message) = self.session.messages.pop() else {
            return;
        };
        let key = message
            .item_id
            .clone()
            .unwrap_or_else(|| format!("event:{index}"));
        match method.as_str() {
            "item/commandExecution/outputDelta"
            | "item/fileChange/outputDelta"
            | "item/reasoning/summaryTextDelta"
            | "item/reasoning/textDelta" => {
                self.stream(
                    index,
                    key,
                    message.role,
                    if message.title.is_empty() {
                        "output".into()
                    } else {
                        message.title
                    },
                    message.text,
                    false,
                );
            }
            "item/reasoning/summaryPartAdded" => {
                if self
                    .streams
                    .get(&key)
                    .is_some_and(|stream| !stream.text.is_empty() && !stream.text.ends_with('\n'))
                {
                    self.stream(
                        index,
                        key,
                        "THOUGHT",
                        "reasoning".into(),
                        "\n".into(),
                        false,
                    );
                }
            }
            "item/started" | "item/completed" => {
                let item = &params["item"];
                let kind = item["type"].as_str().unwrap_or("item");
                if kind == "reasoning" {
                    if message.text.is_empty() && method == "item/started" {
                        self.streams.entry(key.clone()).or_default().segments += 1;
                        self.push(index, Some(key), message.role, message.title, message.text);
                    } else {
                        self.stream(index, key, message.role, message.title, message.text, true);
                    }
                } else {
                    let phase = if method == "item/started" {
                        "started"
                    } else {
                        "finished"
                    };
                    let title = match kind {
                        "commandExecution" => format!(
                            "command · {phase}{}",
                            item["exitCode"]
                                .as_i64()
                                .map(|exit| format!(" · exit {exit}"))
                                .unwrap_or_default()
                        ),
                        "fileChange" => format!("files · {phase}"),
                        "webSearch" => format!("web search · {phase}"),
                        _ => format!("{kind} · {phase}"),
                    };
                    let stream = self.streams.entry(key).or_default();
                    let output = if stream.text.starts_with(&message.text) {
                        String::new()
                    } else {
                        message
                            .text
                            .strip_prefix(&stream.text)
                            .unwrap_or(&message.text)
                            .to_owned()
                    };
                    stream.text = merge_snapshot(&stream.text, &message.text);
                    let text = if kind == "commandExecution" && method == "item/started" {
                        item["command"].as_str().unwrap_or("").to_owned()
                    } else {
                        output
                    };
                    self.push(index, None, message.role, title, text);
                }
            }
            _ => self.push(index, None, message.role, message.title, message.text),
        }
    }

    fn direction(&self, incoming: bool) -> String {
        if incoming {
            format!("{} → Codex", self.client)
        } else {
            format!("Codex → {}", self.client)
        }
    }

    fn visible_output(&self) -> String {
        if self.session.output_level == OutputLevel::Everything {
            return self.session.native.iter().zip(&self.stamps).map(|(record, stamp)| {
                json!({"sequence":stamp.sequence,"elapsed_ms":stamp.elapsed.as_millis(),"direction":self.direction(stamp.incoming),"method":record.method,"params":record.params}).to_string()
            }).collect::<Vec<_>>().join("\n");
        }
        self.rows
            .iter()
            .filter_map(|row| {
                let presentation = self.session.output_level.presentation(&row.message);
                (presentation != MessagePresentation::Hidden).then(|| {
                    format!(
                        "#{:04} {} · {}\n{}",
                        row.stamp.sequence,
                        self.direction(row.stamp.incoming),
                        row.message.title,
                        if presentation == MessagePresentation::Summary
                            && !row.message.progress_expanded
                        {
                            ""
                        } else {
                            &row.message.text
                        }
                    )
                })
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}
